#!/usr/bin/env nu
# Classic mode against the real nix: for every case, run the real
# nix-instantiate/nix-build and ours and compare stdout byte for byte and the
# exit code. Needs a live nix-daemon and the real `nix` on PATH, and it writes
# to the store: `nix flake check` runs it in a container (nix/e2e-test.nix),
# and by hand it runs in the dev shell, which has nix 2.34.8.
#
# usage: nu scripts/classic-e2e.nu
#        BIN=target/debug nu scripts/classic-e2e.nu
use lib.nu [chomp current-system isolate iets-bin make-farm nixpkgs real-bin-dir require-real-nix scratch-daemon show verdict]

def --wrapped run-both [farm: string, name: string, --cwd: string = ".", ...args: string] {
  let ours = (^$"($farm)/($name)" ...$args | complete)
  let real = (^$name ...$args | complete)
  {ours: $ours, real: $real}
}

# `cmd` with `DIR/\xff.nix` as its last argument, through bash: a nushell
# string cannot hold the byte.
def --wrapped with-ff [dir: string, ...cmd: string] {
  ^bash -c r#'d=$1; shift; exec "$@" "$d"/$'\xff'.nix'# _ $dir ...$cmd | complete
}

# Compares stdout bytes and exit code; prints ok/FAIL and returns the verdict.
def cmp-case [label: string, r: record]: nothing -> bool {
  let same_out = ($r.ours.stdout | into binary) == ($r.real.stdout | into binary)
  let same_code = $r.ours.exit_code == $r.real.exit_code
  if $same_out and $same_code {
    verdict true $label ""
  } else {
    let d = $"($label): exit ours=($r.ours.exit_code) real=($r.real.exit_code)"
    print $"FAIL ($d)"
    print $"  ours stdout: ($r.ours.stdout | str substring 0..400)"
    print $"  real stdout: ($r.real.stdout | str substring 0..400)"
    print $"  ours stderr: ($r.ours.stderr | str substring 0..400)"
    print $"  real stderr: ($r.real.stderr | str substring 0..400)"
    false
  }
}

# The first level-0 `--log-format internal-json` error record on stderr, with
# the fields both sides fill alike: Nix colours raw_msg, iets does not.
def err-record [stderr: string]: nothing -> record {
  let recs = ($stderr | lines | where ($it | str starts-with "@nix ") | each { str substring 5.. | from json })
  let r = ($recs | where {|r| $r.level? == 0 and ($r.raw_msg? != null) } | get -o 0 | default {})
  {raw_msg: ($r.raw_msg? | default "" | ansi strip), line: $r.line?, column: $r.column?, file: $r.file?, code: $r.code?}
}

# Compares the error records and the exit codes; ours must carry CODE.
def cmp-record [label: string, code: string, r: record]: nothing -> bool {
  let a = (err-record $r.real.stderr | reject code)
  let b = (err-record $r.ours.stderr)
  if $a == ($b | reject code) and $b.code == $code and $r.ours.exit_code == $r.real.exit_code and $a.raw_msg != "" {
    verdict true $label ""
  } else {
    print $"FAIL ($label): exit ours=($r.ours.exit_code) real=($r.real.exit_code)"
    print $"  real: ($a | to json -r)"
    print $"  ours: ($b | to json -r)"
    false
  }
}

# The structure of a build's activity records: the starts and stops of the
# daemon worker's realise, builds and copy-paths activities and of each
# build, in order, and which result types each activity type got. Ids,
# timings, texts, the substituter queries and iets's own upload ("copying N
# paths", which has a text) are left out.
def act-shape [stderr: string]: nothing -> record {
  let recs = ($stderr | lines | where ($it | str starts-with "@nix ") | each { str substring 5.. | from json })
  let kept = ($recs | where {|r| $r.action == "start" and $r.type in [102 103 104 105] and ($r.type == 105 or $r.text == "") })
  let ty = ($kept | reduce -f {} {|a, m| $m | insert ($a.id | into string) $a.type })
  let ids = ($ty | columns)
  let mine = ($recs | where {|r| ($r.id? | default "" | into string) in $ids })
  {
    order: ($mine | where action in [start stop] | each {|r| $"($r.action) ($ty | get ($r.id | into string))" })
    results: ($mine | where action == result | each {|r| $"($ty | get ($r.id | into string))/($r.type)" } | uniq | sort)
  }
}

# `env` output is only comparable without the variables that legitimately
# differ: the per-run temporary directory, bash's own bookkeeping, the prompt.
const ENV_SKIP = '^(NIX_BUILD_TOP|TMPDIR|TMP|TEMP|TEMPDIR|NIX_ATTRS_SH_FILE|NIX_ATTRS_JSON_FILE|__[A-Za-z_]*|SHLVL|_|PWD|OLDPWD|PS1)='

def env-filter [text: string]: nothing -> string {
  $text | lines | where not ($it =~ $ENV_SKIP) | sort | str join "\n"
}

# Compares a filtered `env` dump and the exit code.
def cmp-env [label: string, r: record]: nothing -> bool {
  let a = (env-filter $r.real.stdout)
  let b = (env-filter $r.ours.stdout)
  if $a == $b and $r.ours.exit_code == $r.real.exit_code {
    verdict true $"($label) \(($a | lines | length) variables\)" ""
  } else {
    print $"FAIL ($label): exit ours=($r.ours.exit_code) real=($r.real.exit_code)"
    print $"  only real: ($a | lines | where $it not-in ($b | lines) | first 4 | str join ' | ')"
    print $"  only ours: ($b | lines | where $it not-in ($a | lines) | first 4 | str join ' | ')"
    print $"  ours stderr: ($r.ours.stderr | str substring 0..300)"
    false
  }
}

# The reason a case falls back, without paying for the real run. `cmd` is the
# classic name and its arguments.
def fallback-reason [farm: string, cmd: list<string>]: nothing -> string {
  let name = ($cmd | first)
  let args = ($cmd | skip 1)
  (with-env {IETS_NO_FALLBACK: "1"} { ^$"($farm)/($name)" ...$args | complete }).stderr
}

# The tracing cache under the `nix` name (docs/reference/classic.md). The
# fixture's value is `val.nix`, a file of a tarball input and, when impure, an
# environment variable, behind a `builtins.trace`: an evaluation prints the
# trace line and a hit does not, which is the only way to tell them apart,
# because a hit prints what the evaluation printed. Each command in COMMANDS
# goes through the same steps: cold, hit, an edit, another lock, `--impure`
# with a variable that changes, and `--override-input`, which is not iets's.
def cache-cases [farm: string, tmp: string, np: string, sys: string]: nothing -> list<bool> {
  let cc = $"($tmp)/cc"
  mkdir $"($cc)/flake" $"($cc)/tbA" $"($cc)/tbB"
  "A" | save $"($cc)/tbA/a"
  "B" | save $"($cc)/tbB/a"
  ^tar -C $cc -czf $"($cc)/tbA.tar.gz" tbA
  ^tar -C $cc -czf $"($cc)/tbB.tar.gz" tbB
  r#'{
  inputs.nixpkgs.url = "path:@NIXPKGS@";
  inputs.tb = { url = "file://@CC@/tbA.tar.gz"; flake = false; };
  outputs = { self, nixpkgs, tb }:
    let
      system = "@SYS@";
      val = builtins.trace "EVALUATED"
        (import ./val.nix + "-" + builtins.readFile "${tb}/a" + "-" + builtins.getEnv "CC_VAR");
    in {
      inherit val;
      packages.${system}.pkg = derivation {
        name = "cc-pkg";
        inherit system val;
        stamp = self.narHash;
        builder = "/bin/sh";
        args = [ "-c" "echo $val > $out" ];
      };
      devShells.${system}.default = nixpkgs.legacyPackages.${system}.mkShell { inherit val; stamp = self.narHash; };
    };
}
'# | str replace -a "@NIXPKGS@" $np | str replace -a "@CC@" $cc | str replace -a "@SYS@" $sys | save $"($cc)/flake/flake.nix"
  let ref = $"path:($cc)/flake"
  let lock_a = $"($cc)/flake/flake.lock"
  mut res = []
  let locked = (do { cd $"($cc)/flake"; ^nix flake lock } | complete)
  $res ++= [(verdict ($locked.exit_code == 0) "nix flake lock for the cache fixture" $locked.stderr)]
  # the lock `nix flake update tb` would write after the tarball moved on
  let hash_b = (^nix hash path --type sha256 --sri $"($cc)/tbB" | chomp)
  let lock_b = (open --raw $lock_a | from json
    | update nodes.tb.locked.url $"file://($cc)/tbB.tar.gz"
    | update nodes.tb.locked.narHash $hash_b | to json)

  let commands = [
    {name: "nix eval", args: [eval --raw], attr: "#val", tail: [], value: {|r| $r.stdout }}
    {name: "nix build", args: [build --no-link --print-out-paths], attr: "#pkg", tail: []
     value: {|r| try { open --raw ($r.stdout | chomp) } catch { "" } }}
    {name: "nix derivation show", args: [derivation show], attr: "#pkg", tail: []
     value: {|r| try { $r.stdout | from json | get derivations | values | get 0.env.val } catch { "" } }}
    {name: "nix develop", args: [develop], attr: "", tail: [-c sh -c "echo \"$val\""], value: {|r| $r.stdout }}
  ]
  let ran = {|r| $r.stderr | str contains "trace: EVALUATED" }
  $env.IETS_CACHE_DIR = $"($cc)/cache"
  for c in $commands {
    let one = {|flags: list<string>| ^$"($farm)/nix" ...$c.args ...$flags $"($ref)($c.attr)" ...$c.tail | complete }
    let entries = {|| try { ls $"($cc)/cache/trace" | where type == dir | length } catch { 0 } }
    "\"one\"\n" | save -f $"($cc)/flake/val.nix"
    open --raw $lock_a | save -f $"($cc)/lock-a"

    let cold = (do $one [])
    let real = (^nix ...$c.args $"($ref)($c.attr)" ...$c.tail | complete)
    $res ++= [((cmp-case $"($c.name): a cold run prints what nix prints" {ours: $cold, real: $real})
      and (verdict ((do $ran $cold) and (do $c.value $cold | str contains "one-A-")) $"($c.name): a cold run evaluates" $"stderr ($cold.stderr | str substring 0..300)"))]
    let hit = (do $one [])
    $res ++= [((cmp-case $"($c.name): a hit prints what nix prints" {ours: $hit, real: $real})
      and (verdict ((not (do $ran $hit)) and ($hit.stdout | into binary) == ($cold.stdout | into binary))
        $"($c.name): the second run is a hit with the same bytes" $"stderr ($hit.stderr | str substring 0..300)"))]
    # `stamp` is what `tree-meta-in-drv` reports under the iets names
    $res ++= [(verdict (not (($cold.stderr + $hit.stderr) | str contains "warning: ["))
      $"($c.name): no lint under a classic name" $"stderr ($cold.stderr | str substring 0..300)")]
    # a flag or a setting with no effect has none on a hit either, so the
    # warnings come from the command line, not from the evaluation
    let noisy = {|| with-env {NIX_CONFIG: "accept-flake-config = true\nno-such-setting = 1"} { do $one [--accept-flake-config --eval-store auto] } }
    do $noisy | ignore
    let warned = (do $noisy)
    let want = ["'--accept-flake-config' is ignored:" "'--eval-store' is ignored:" "setting 'accept-flake-config' is ignored:" "unknown setting 'no-such-setting'"]
    let each_once = ($want | all {|w| ($warned.stderr | lines | where ($it | str starts-with $"warning: ($w)") | length) == 1 })
    $res ++= [(verdict ((not (do $ran $warned)) and $each_once and $warned.exit_code == 0)
      $"($c.name): a hit gives every warning once" $"stderr ($warned.stderr | str substring 0..600)")]

    "\"two\"\n" | save -f $"($cc)/flake/val.nix"
    let jlog = $"($cc)/jlog"
    rm -f $jlog
    let edit = (do $one [--option json-log-path $jlog])
    let recs = (try { open --raw $jlog | lines | each { from json } | where code? == "cache-invalidated" | length } catch { 0 })
    $res ++= [(verdict ((do $ran $edit) and (do $c.value $edit | str contains "two-A-") and (not (do $ran (do $one []))))
      $"($c.name): an edit of a file the flake reads evaluates again, and then hits" $"value ((do $c.value $edit)), stderr ($edit.stderr | str substring 0..300)")]
    $res ++= [(verdict ((not ($edit.stderr | str contains "re-evaluating")) and $recs == 1)
      $"($c.name): the invalidation is silent on stderr and one record in json-log-path" $"($recs) records, stderr ($edit.stderr | str substring 0..300)")]

    $lock_b | save -f $lock_a
    let relock = (do $one [])
    $res ++= [(verdict ((do $ran $relock) and (do $c.value $relock | str contains "two-B-") and (not (do $ran (do $one []))))
      $"($c.name): another flake.lock evaluates again, and then hits" $"value ((do $c.value $relock)), stderr ($relock.stderr | str substring 0..300)")]
    open --raw $"($cc)/lock-a" | save -f $lock_a

    let imp_a = (with-env {CC_VAR: "a"} { do $one [--impure] })
    let imp_hit = (with-env {CC_VAR: "a"} { do $one [--impure] })
    let imp_b = (with-env {CC_VAR: "b"} { do $one [--impure] })
    $res ++= [(verdict ((do $ran $imp_a) and (do $c.value $imp_a | str contains "two-A-a") and (not (do $ran $imp_hit))
        and (do $ran $imp_b) and (do $c.value $imp_b | str contains "two-A-b"))
      $"($c.name) --impure: its own entry, and a variable that changed evaluates again" $"a ((do $c.value $imp_a)), b ((do $c.value $imp_b)), hit stderr ($imp_hit.stderr | str substring 0..200)")]

    let before = (do $entries)
    let over = (fallback-reason $farm [nix ...$c.args --override-input tb $"path:($cc)/tbB" $"($ref)($c.attr)" ...$c.tail])
    $res ++= [(verdict (($over | str contains "'--override-input' is not implemented") and (do $entries) == $before)
      $"($c.name) --override-input is nix's: no entry is read or written" $"stderr ($over), ($before) -> ((do $entries)) entries")]
  }

  # a hit skips the evaluation and nothing else: the out-link, the root of a
  # `--no-link` build and the root of a shell are made again when they are
  # gone. The first runs bring the entries back to the lock the loop restored.
  let roots = {|| try { ls $"($cc)/cache/roots" | length } catch { 0 } }
  ^$"($farm)/nix" build --no-link $"($ref)#pkg" | complete | ignore
  ^$"($farm)/nix" develop $ref -c true | complete | ignore
  rm -rf $"($cc)/cache/roots"
  let nolink = (^$"($farm)/nix" build --no-link --print-out-paths $"($ref)#pkg" | complete)
  $res ++= [(verdict ((not (do $ran $nolink)) and (do $roots) == 1)
    "nix build --no-link: a hit keeps the root" $"((do $roots)) roots, stderr ($nolink.stderr | str substring 0..200)")]
  let linked = (^$"($farm)/nix" build -o $"($cc)/res" $"($ref)#pkg" | complete)
  let target = (try { ^readlink $"($cc)/res" | chomp } catch { "" })
  $res ++= [(verdict ((not (do $ran $linked)) and $target == ($nolink.stdout | chomp) and ($target | path exists))
    "nix build -o: a hit makes the out-link" $"link -> ($target), stderr ($linked.stderr | str substring 0..200)")]
  rm -rf $"($cc)/cache/roots"
  let shell = (^$"($farm)/nix" develop $ref -c true | complete)
  $res ++= [(verdict ((not (do $ran $shell)) and $shell.exit_code == 0 and (do $roots) == 1)
    "nix develop: a hit keeps the root" $"((do $roots)) roots, stderr ($shell.stderr | str substring 0..200)")]
  hide-env IETS_CACHE_DIR
  $res
}

def main [] {
  require-real-nix nix nix-instantiate nix-build nix-shell
  let iets = (iets-bin)
  let tmp = (isolate classic-e2e)
  let np = (nixpkgs)
  let real = (real-bin-dir)
  print $"real nix: ($real)"
  $env.IETS_NIX_BIN_DIR = $real
  let farm = (make-farm $iets $tmp)
  mut ok = []

  # --- the builtins surface: the same names Nix has ----------------------------
  # Under a classic name the three builtins Nix gates on an experimental feature
  # are gated here too (`builtins::exposed`), so the sets are equal whatever the
  # features are. iets's own commands keep every feature on, which is the
  # documented divergence (docs/reference/divergences.md).
  for mode in ["--impure" "--pure-eval"] {
    for feats in ["" "dynamic-derivations" "dynamic-derivations fetch-closure fetch-tree"] {
      let b = (run-both $farm nix-instantiate --option experimental-features $"nix-command flakes ($feats)"
        --eval --json -E "builtins.attrNames builtins" $mode)
      let ours = (try { $b.ours.stdout | from json } catch { [] })
      let real = (try { $b.real.stdout | from json } catch { [] })
      let missing = ($real | where {|n| $n not-in $ours })
      let extra = ($ours | where {|n| $n not-in $real })
      $ok = ($ok | append (verdict (($missing | is-empty) and ($extra | is-empty))
        $"builtins ($mode) [($feats)]: the same names nix has"
        $"missing ($missing | str join ' '), extra ($extra | str join ' ')"))
    }
  }

  # --- nix-instantiate: drv paths
  $ok = ($ok | append (cmp-case "instantiate <nixpkgs> -A hello"
    (run-both $farm nix-instantiate -I $"nixpkgs=($np)" "<nixpkgs>" -A hello)))
  $ok = ($ok | append (cmp-case "instantiate <nixpkgs> -A glibc (multi-output)"
    (run-both $farm nix-instantiate -I $"nixpkgs=($np)" "<nixpkgs>" -A glibc)))
  $ok = ($ok | append (cmp-case "instantiate <nixpkgs> -A glibc.dev (!output suffix)"
    (run-both $farm nix-instantiate -I $"nixpkgs=($np)" "<nixpkgs>" -A glibc.dev)))
  $ok = ($ok | append (cmp-case "instantiate two -A in one run"
    (run-both $farm nix-instantiate -I $"nixpkgs=($np)" "<nixpkgs>" -A hello -A glibc.dev)))
  # one getDerivations per attribute path, so one derivation named twice is
  # printed twice and gets two roots
  $ok = ($ok | append (cmp-case "instantiate two -A that name one derivation"
    (run-both $farm nix-instantiate -I $"nixpkgs=($np)" "<nixpkgs>" -A hello -A hello)))
  # an empty --add-root is "no root": the drv path and the GC warning
  $ok = ($ok | append (cmp-case "instantiate --add-root ''"
    (run-both $farm nix-instantiate --add-root "" -I $"nixpkgs=($np)" "<nixpkgs>" -A hello)))

  # --- nix-instantiate: --eval
  $ok = ($ok | append (cmp-case "instantiate --eval --strict --json"
    (run-both $farm nix-instantiate --eval --strict --json -E "{ a = 1; }")))
  $ok = ($ok | append (cmp-case "instantiate --eval"
    (run-both $farm nix-instantiate --eval -E "{ a = 1; b = [ 1 2 ]; }")))
  $ok = ($ok | append (cmp-case "instantiate --eval --raw"
    (run-both $farm nix-instantiate --eval --raw -E "\"x\" + \"y\"")))
  # a path value is copied to the store first, as coerceToString's default does
  let rawp = $"($tmp)/raw"
  mkdir $rawp
  "raw-path\n" | save -f $"($rawp)/p.txt"
  $ok = ($ok | append (cmp-case "instantiate --eval --raw a path"
    (run-both $farm nix-instantiate --eval --raw -E $"($rawp)/p.txt")))
  $ok = ($ok | append (cmp-case "instantiate --eval --raw an outPath"
    (run-both $farm nix-instantiate --eval --raw -E $"\({ outPath = ($rawp)/p.txt; }).outPath")))
  # autoArgs is a map: one name given twice keeps the last value
  $ok = ($ok | append (cmp-case "instantiate --eval, one --arg name twice"
    (run-both $farm nix-instantiate --eval --arg a 1 --arg a 2 -E "{ a }: a")))
  $ok = ($ok | append (cmp-case "instantiate --eval, --arg then --argstr of one name"
    (run-both $farm nix-instantiate --eval --arg a 1 --arg b 9 --argstr a x -E "{ a, b }: [ a b ]")))
  # each value is written as it is made, so what ran before a failure is out
  $ok = ($ok | append (cmp-case "instantiate --eval, a later expression throws"
    (run-both $farm nix-instantiate --eval -E "1" "throw \"x\"")))
  $ok = ($ok | append (cmp-case "instantiate --eval --argstr"
    (run-both $farm nix-instantiate --eval --strict --json --argstr who world -E "{ who }: { inherit who; }")))

  # --- a tiny custom derivation, like scripts/daemon-e2e.nu case 1
  let d = $"($tmp)/case"
  mkdir $"($d)/src"
  $"hello ($nu.pid)\n" | save $"($d)/src/a"
  r#'derivation {
  name = "classic-e2e";
  system = "x86_64-linux";
  builder = "/bin/sh";
  args = [ "-c" "cat ${@DIR@/src}/a > $out" ];
}
'# | str replace -a "@DIR@" $d | save $"($d)/default.nix"
  r#'derivation {
  name = "classic-e2e-multi";
  system = "x86_64-linux";
  outputs = [ "out" "dev" ];
  builder = "/bin/sh";
  args = [ "-c" "echo a > $out; echo b > $dev" ];
}
'# | save $"($d)/multi.nix"

  # a `=drv` context closure over a toFile text that names another toFile
  r#'let
  a = builtins.toFile "classic-e2e-tf-a" "x";
  b = builtins.toFile "classic-e2e-tf-b" "ref ${a}";
  c = builtins.toFile "classic-e2e-tf-c" "ref ${b}";
  dep = name: src: derivation { inherit name src; system = "x86_64-linux"; builder = "/bin/sh"; };
in [
  (derivation { name = "classic-e2e-tf-1"; system = "x86_64-linux"; builder = "/bin/sh"; x = (dep "classic-e2e-tf-in1" b).drvPath; })
  (derivation { name = "classic-e2e-tf-2"; system = "x86_64-linux"; builder = "/bin/sh"; x = (dep "classic-e2e-tf-in2" c).drvPath; })
]
'# | save $"($d)/textrefs.nix"

  # nlohmann writes a structuredAttrs float in its own shortest form
  r#'derivation {
  name = "classic-e2e-floats";
  system = "x86_64-linux";
  builder = "/bin/sh";
  __structuredAttrs = true;
  a = 0.00001;
  b = 1.0e15;
  c = 1.234e-05;
  d = 1.0e-7;
  e = 2.5;
  f = 1.0;
  g = [ 0.0001 { h = 1.0e20; } ];
}
'# | save $"($d)/floats.nix"

  # builtins.outputOf over a text-addressed producer, once and nested, and a
  # consumer of both: its inputDrvs are a tree and its ATerm is the versioned
  # form, so the printed drvPath covers the whole chain
  r#'let
  producer = name: derivation {
    inherit name;
    system = "x86_64-linux";
    builder = "/bin/sh";
    args = [ "-c" "echo done > $out" ];
    __contentAddressed = true;
    outputHashMode = "text";
    outputHashAlgo = "sha256";
  };
  one = builtins.outputOf (producer "classic-e2e-p.drv").outPath "out";
  two = builtins.outputOf (builtins.outputOf (producer "classic-e2e-q.drv.drv").outPath "out") "out";
in derivation {
  name = "classic-e2e-dyn";
  system = "x86_64-linux";
  builder = "/bin/sh";
  args = [ "-c" "echo ${one} ${two} > $out" ];
}
'# | save $"($d)/dyndrv.nix"

  # Two consumers of one producer that differ only in which dynamic output
  # they take. `hash_derivation_modulo` drops the children of an input, so
  # this is the pair that would collide if that mattered: it does not, because
  # the producer makes both consumers deferred and the placeholder is in the
  # env the modulo ATerm hashes (docs/reference/divergences.md).
  r#'let
  producer = derivation {
    name = "classic-e2e-h.drv";
    system = "x86_64-linux";
    builder = "/bin/sh";
    args = [ "-c" "echo done > $out" ];
    __contentAddressed = true;
    outputHashMode = "text";
    outputHashAlgo = "sha256";
  };
  cons = out: derivation {
    name = "classic-e2e-h-cons";
    system = "x86_64-linux";
    builder = "/bin/sh";
    args = [ "-c" "true" ];
    dep = builtins.outputOf producer.outPath out;
  };
  a = cons "outA";
  b = cons "outB";
in { aDrv = a.drvPath; bDrv = b.drvPath; aOut = a.outPath; bOut = b.outPath; }
'# | save $"($d)/dyndeferred.nix"

  $ok = ($ok | append (cmp-case "instantiate a custom drv"
    (run-both $farm nix-instantiate $"($d)/default.nix")))
  $ok = ($ok | append (cmp-case "drv closure follows the references of a toFile"
    (run-both $farm nix-instantiate $"($d)/textrefs.nix")))
  $ok = ($ok | append (cmp-case "instantiate builtins.outputOf, once and nested"
    (run-both $farm nix-instantiate --extra-experimental-features "ca-derivations dynamic-derivations"
      $"($d)/dyndrv.nix")))
  $ok = ($ok | append (cmp-case "eval builtins.outputOf placeholders"
    (run-both $farm nix-instantiate --extra-experimental-features "ca-derivations dynamic-derivations"
      --read-write-mode --eval --strict --json -E
      $"let f = import ($d)/dyndrv.nix; in f.drvPath")))
  $ok = ($ok | append (cmp-case "two consumers of one dynamic producer stay apart"
    (run-both $farm nix-instantiate --extra-experimental-features "ca-derivations dynamic-derivations"
      --read-write-mode --eval --strict --json $"($d)/dyndeferred.nix")))
  $ok = ($ok | append (cmp-case "instantiate --readonly-mode"
    (run-both $farm nix-instantiate --readonly-mode $"($d)/default.nix")))

  # --- --add-root: the printed path is the link, and the daemon knows the root
  mut root_ok = true
  for who in [real ours] {
    let cmd = if $who == "real" { "nix-instantiate" } else { $"($farm)/nix-instantiate" }
    let link = $"($tmp)/gcroot-($who)"
    let out = (^$cmd --add-root $link $"($d)/default.nix" | complete)
    let printed = ($out.stdout | chomp)
    if $printed != $link { print $"FAIL --add-root ($who): printed '($printed)' != '($link)'"; $root_ok = false }
    let target = (^readlink $link | chomp)
    let roots = (^nix-store --query --roots $target | complete | get stdout)
    if not ($roots | str contains $link) { print $"FAIL --add-root ($who): ($link) is not a gc root"; $root_ok = false }
  }
  $ok = ($ok | append (verdict $root_ok "--add-root links and registers a gc root" "see above"))

  # --- an out-link replaces only a symlink that already points into the store
  # (indirect-root-store.cc:addPermRoot); everything else stays as it is
  mut keep_ok = true
  for kind in [regular dir other-link dangling] {
    mut got = {}
    for who in [real ours] {
      let wd = $"($tmp)/keep-($kind)-($who)"
      mkdir $wd
      cd $wd
      match $kind {
        "regular" => { "PRECIOUS\n" | save -f t }
        "dir" => { mkdir t }
        "other-link" => { ^ln -s /etc/hostname t }
        _ => { ^ln -s /nonexistent-for-e2e t }
      }
      let cmd = if $who == "real" { "nix-build" } else { $"($farm)/nix-build" }
      let r = (^$cmd -o t $"($d)/default.nix" | complete)
      let link = (^readlink t | complete | get stdout | chomp)
      let isdir = ((^test -d t | complete | get exit_code) == 0)
      $got = ($got | insert $who {code: $r.exit_code, link: $link, isdir: $isdir})
    }
    if $got.ours != $got.real {
      print $"FAIL out-link over a ($kind): ours ($got.ours), real ($got.real)"
      $keep_ok = false
    }
  }
  $ok = ($ok | append (verdict $keep_ok "-o over an existing path acts as Nix does" "see above"))

  # --- nix-build: default `result`, -o, multi-output naming
  for case in [
    {label: "build a custom drv, default result", args: [$"($d)/default.nix"], links: [result]}
    {label: "build -o link", args: [-o link $"($d)/default.nix"], links: [link]}
    {label: "build --no-out-link", args: [--no-out-link $"($d)/default.nix"], links: []}
    {label: "build two outputs of one drv", args: [-E $"let m = import ($d)/multi.nix; in [ m.dev m.out ]"], links: [result result-dev]}
    {label: "build two drvs", args: [-E ("[ (import " + $d + "/default.nix) (import " + $d + "/multi.nix) ]")], links: [result result-2]}
  ] {
    mut got = {}
    for who in [real ours] {
      let wd = $"($tmp)/build-($who)"
      mkdir $wd
      cd $wd
      let cmd = if $who == "real" { "nix-build" } else { $"($farm)/nix-build" }
      let r = (^$cmd ...$case.args | complete)
      let links = ($case.links | each {|l| if ($l | path exists) { ^readlink $l | chomp } else { $"MISSING ($l)" } })
      $got = ($got | insert $who {out: ($r.stdout), code: $r.exit_code, links: $links, err: $r.stderr})
    }
    let same = (($got.ours.out | into binary) == ($got.real.out | into binary)) and ($got.ours.code == $got.real.code) and ($got.ours.links == $got.real.links)
    if $same {
      print $"ok   ($case.label)"
    } else {
      print $"FAIL ($case.label)"
      print $"  ours: code=($got.ours.code) links=($got.ours.links)"
      print $"  real: code=($got.real.code) links=($got.real.links)"
      print $"  ours stdout: ($got.ours.out)"
      print $"  real stdout: ($got.real.out)"
      print $"  ours stderr: ($got.ours.err | str substring 0..400)"
    }
    $ok = ($ok | append $same)
  }

  # --- the `builder` setting: the build runs on a scratch chroot daemon, and
  # the outputs come back so `result` names a path the local store has. The
  # builder uses shell builtins only: a chroot store has no coreutils.
  r#'derivation {
  name = "classic-e2e-builder";
  system = "x86_64-linux";
  builder = "/bin/sh";
  args = [ "-c" "while read -r l; do echo \"$l\"; done < ${@DIR@/src}/a > $out" ];
}
'# | str replace -a "@DIR@" $d | save $"($d)/builder.nix"
  let sd = $"($tmp)/scratch"
  mkdir $sd
  let sock = (scratch-daemon $sd)
  let wd = $"($tmp)/build-builder"
  mkdir $wd
  cd $wd
  let r = (with-env {IETS_CONFIG: $'builder = "unix://($sock)"'} {
    ^$"($farm)/nix-build" $"($d)/builder.nix" | complete
  })
  let printed = ($r.stdout | chomp)
  let link = (if ("result" | path exists) { ^readlink result | chomp } else { "MISSING" })
  let built_there = ($"($sd)($printed)" | path exists)
  let here = ((^nix-store --check-validity $printed | complete).exit_code == 0)
  let rooted = ((^nix-store --query --roots $printed | complete).stdout | str contains $"($wd)/result")
  let builder_ok = ($r.exit_code == 0) and ($link == $printed) and $built_there and $here and $rooted
  if not $builder_ok {
    print $"  exit=($r.exit_code) printed=($printed) link=($link)"
    print $"  on the scratch daemon=($built_there) valid here=($here) gc root=($rooted)"
    print $"  stderr: ($r.stderr | str substring 0..600)"
  }
  $ok = ($ok | append (verdict $builder_ok "builder = unix://SCRATCH builds there and registers here" "see above"))
  cd $tmp

  # --- fallbacks
  $ok = ($ok | append (cmp-case "--parse execs the real nix-instantiate"
    (run-both $farm nix-instantiate --parse -E 1)))
  # `--iets-option` is ours: the fallback must exec nix without it.
  $ok = ($ok | append (cmp-case "--iets-option never reaches the fallback nix" {
    ours: (^$"($farm)/nix-instantiate" --iets-option workers 2 --parse -E 1 | complete),
    real: (^nix-instantiate --parse -E 1 | complete)
  }))
  let fb = (^$"($farm)/nix-instantiate" --parse -E 1 | complete)
  $ok = ($ok | append (verdict ($fb.stderr | str contains "iets: falling back to nix-instantiate:")
    "the fallback prints one line on stderr" $"stderr was '($fb.stderr)'"))

  let nofb = (with-env {IETS_NO_FALLBACK: "1"} { ^$"($farm)/nix-instantiate" --parse -E 1 | complete })
  $ok = ($ok | append (verdict ($nofb.exit_code == 1) "IETS_NO_FALLBACK=1 exits 1" $"exit ($nofb.exit_code)"))

  # nix-bin-dir naming a farm of iets's own classic names: every candidate is
  # iets, so the fallback must stop instead of exec'ing iets again
  let loop_dir = $"($tmp)/loop"
  mkdir $"($loop_dir)/bin"
  for n in [nix-instantiate nix-build nix-eval-jobs nix-shell nix] { ^ln -sf $iets $"($loop_dir)/bin/($n)" }
  let timeout_bin = (which timeout | get 0.path)
  let looped = (with-env {IETS_NIX_BIN_DIR: $"($loop_dir)/bin", PATH: $"($loop_dir)/bin"} {
    ^$timeout_bin 20 $"($farm)/nix-instantiate" --parse -E 1 | complete
  })
  $ok = ($ok | append (verdict
    ($looped.exit_code == 1 and ($looped.stderr | str contains "cannot fall back to nix-instantiate: not found on PATH"))
    "nix-bin-dir full of iets: the fallback stops, it does not loop"
    $"exit ($looped.exit_code), stderr ($looped.stderr | str substring 0..300)"))

  # a write that fails on stdout: `nix eval` reports it and exits 1, the
  # legacy names exit 0 as `std::cout` lets them, and nothing aborts
  let full = {|cmd: list<string>| (^bash -c r#'"$@" > /dev/full 2>/dev/null'# _ ...$cmd | complete).exit_code }
  for case in [
    {label: "nix eval > /dev/full", args: [nix eval --expr "1"], want: 1}
    {label: "nix-instantiate --eval > /dev/full", args: [nix-instantiate --eval -E "1"], want: 0}
  ] {
    let ours = (do $full ([$"($farm)/($case.args | first)"] ++ ($case.args | skip 1)))
    let real = (do $full $case.args)
    $ok = ($ok | append (verdict ($ours == $case.want and $real == $case.want) $case.label
      $"($case.label): ours ($ours), real ($real), want ($case.want)"))
  }
  for c in [[version], [parse, $"($d)/multi.nix"], [--help], [lint, list]] {
    let code = (do $full ([$iets] ++ $c))
    $ok = ($ok | append (verdict ($code == 1) $"iets ($c | str join ' ') > /dev/full reports, not aborts" $"exit ($code)"))
  }

  # an iets.toml with a mistake in it stops a command that reads a setting,
  # and not `version` or the help, which read none
  let badconf = $"($tmp)/badconf"
  mkdir $"($badconf)/iets"
  "bogus = 1\n" | save -f $"($badconf)/iets/iets.toml"
  for case in [{args: [version], want: 0} {args: [--help], want: 0} {args: [eval, -E, "1"], want: 1}] {
    let r = (with-env {XDG_CONFIG_HOME: $badconf} { ^$iets ...$case.args | complete })
    $ok = ($ok | append (verdict ($r.exit_code == $case.want)
      $"a bad iets.toml and 'iets ($case.args | str join ' ')'"
      $"exit ($r.exit_code), want ($case.want): ($r.stderr)"))
  }
  # the log format is read before anything can fail, so an early error is a record
  let rec = (with-env {XDG_CONFIG_HOME: $badconf} {
    ^$iets eval --log-format internal-json -E "1" | complete })
  $ok = ($ok | append (verdict ($rec.stderr | str starts-with "@nix ")
    "an early error is a record under internal-json" $"stderr ($rec.stderr | str substring 0..200)"))

  mkdir $"($tmp)/tb"
  "hi\n" | save $"($tmp)/tb/f"
  ^tar -czf $"($tmp)/x.tar.gz" -C $tmp tb
  $ok = ($ok | append (cmp-case "fetchTarball falls back and prints the real result"
    (run-both $farm nix-instantiate --eval -E $"builtins.fetchTarball \"file://($tmp)/x.tar.gz\"")))

  let ifd = $"import \(derivation { name = \"x\"; system = builtins.currentSystem; builder = \"/bin/sh\"; args = [\"-c\" \"echo 42 > $out\"]; })"
  $ok = ($ok | append (cmp-case "import from derivation falls back"
    (run-both $farm nix-instantiate --eval -E $ifd)))
  # --impure: the expression names `builtins.currentSystem`, which a pure
  # evaluation has not got, and the point here is the IFD refusal
  let r = (^$iets eval --impure -E $ifd | complete)
  $ok = ($ok | append (verdict ($r.exit_code == 1 and ($r.stderr | str contains "import from derivation is not supported"))
    "iets eval refuses import from derivation" $"exit ($r.exit_code), stderr '($r.stderr)'"))

  # --- a file name that is not UTF-8
  let ff = $"($tmp)/ff"
  mkdir $ff
  ^bash -c r#'echo "{ a = 1; }" > "$1"/$'\xff'.nix'# _ $ff
  let real_ff = (with-ff $ff nix-instantiate --eval)
  $ok = ($ok | append (cmp-case "instantiate --eval a non-UTF-8 file name"
    {ours: (with-ff $ff $"($farm)/nix-instantiate" --eval), real: $real_ff}))
  $ok = ($ok | append (cmp-case "iets eval a non-UTF-8 file name"
    {ours: (with-ff $ff $iets eval), real: $real_ff}))
  $ok = ($ok | append (cmp-case "a non-UTF-8 file name reaches the fallback intact"
    {ours: (with-ff $ff $"($farm)/nix-instantiate" --parse), real: (with-ff $ff nix-instantiate --parse)}))

  let repl = (^$"($farm)/nix" repl --version | complete)
  $ok = ($ok | append (verdict ($repl.stderr | str contains "iets: falling back to nix:")
    "the 'nix' name falls back" $"stderr was '($repl.stderr)'"))

  # --- the `nix` name: a locked flake with no inputs, copied out of the
  # repository so both sides see a `path:` flake instead of a git one.
  let fdir = $"($tmp)/flake"
  mkdir $fdir
  cp $"($env.FILE_PWD)/../tests/fixtures/flake/flake.nix" $fdir
  cp $"($env.FILE_PWD)/../tests/fixtures/flake/flake.lock" $fdir
  let fref = $"path:($fdir)"

  $ok = ($ok | append (cmp-case "nix eval --json .#x"
    (run-both $farm nix eval --json $"($fref)#x")))
  $ok = ($ok | append (cmp-case "nix eval --raw .#x"
    (run-both $farm nix eval --raw $"($fref)#x")))
  $ok = ($ok | append (cmp-case "nix eval .#s (attrset, list, float)"
    (run-both $farm nix eval $"($fref)#s")))
  $ok = ($ok | append (cmp-case "nix eval .#l"
    (run-both $farm nix eval $"($fref)#l")))

  let pf = (run-both $farm nix flake prefetch-inputs $fref)
  $ok = ($ok | append ((cmp-case "nix flake prefetch-inputs, no inputs" $pf) and (verdict (not ($pf.ours.stderr | str contains "falling back"))
    "nix flake prefetch-inputs is iets's own" $"stderr '($pf.ours.stderr)'")))

  # the farm first on PATH: copying a fresh tree must spawn the real nix, not us
  let fdir2 = $"($tmp)/flake-fresh"
  mkdir $fdir2
  cp $"($fdir)/flake.nix" $fdir2
  cp $"($fdir)/flake.lock" $fdir2
  $"# ($tmp)\n" | save --append $"($fdir2)/flake.nix"
  let ours = (with-env { PATH: $"($farm):($env.PATH)" } { ^$"($farm)/nix" eval --json $"path:($fdir2)#x" | complete })
  let real = (^nix eval --json $"path:($fdir2)#x" | complete)
  $ok = ($ok | append (cmp-case "nix eval, farm first on PATH, fresh tree" {ours: $ours, real: $real}))

  # the farm shadows the real nix out of PATH and no IETS_NIX_BIN_DIR is set:
  # the fallback must find nix through the farm's own marker
  let shadow = $"($tmp)/shadow"
  mkdir $shadow
  ^ln -sf (which bash | get 0.path) $"($shadow)/bash"
  let ours = (with-env { PATH: $"($farm):($shadow)", IETS_NIX_BIN_DIR: null } { ^$"($farm)/nix-instantiate" --parse -E "1 + 1" | complete })
  let real = (^nix-instantiate --parse -E "1 + 1" | complete)
  $ok = ($ok | append (cmp-case "fallback through the farm marker, real nix off PATH" {ours: $ours, real: $real}))
  $ok = ($ok | append (cmp-case "nix eval .#pkg (a derivation)"
    (run-both $farm nix eval $"($fref)#pkg")))
  $ok = ($ok | append (cmp-case "nix eval . (packages.<system>.default)"
    (run-both $farm nix eval $fref)))
  $ok = ($ok | append (cmp-case "nix eval --expr"
    (run-both $farm nix eval --expr "{ a = 1; b = [ 1 2 ]; c = \"x\"; d = 123456789.5; }")))
  # --- pure-eval (docs/reference/settings.md): `nix` is pure unless told otherwise,
  # `--file` is impure, and the legacy names are impure.
  let env_expr = 'builtins.getEnv "HOME"'
  $env_expr | save -f $"($tmp)/env.nix"
  let pure_probe = '[ (builtins ? currentSystem) (builtins.getEnv "HOME") builtins.nixPath ]'
  for case in [
    [label args];
    ["nix eval --expr is pure" [nix eval --expr $env_expr]]
    ["nix eval --impure --expr" [nix eval --impure --expr $env_expr]]
    ["nix --option pure-eval false eval --expr" [nix --option pure-eval false eval --expr $env_expr]]
    ["nix eval -f is impure" [nix eval -f $"($tmp)/env.nix"]]
    ["nix eval .#x is pure" [nix eval $"($fref)#x"]]
    ["nix-instantiate is impure" [nix-instantiate --eval -E $env_expr]]
    ["nix-instantiate --pure-eval" [nix-instantiate --pure-eval --eval --strict -E $pure_probe]]
    ["nix-instantiate --option pure-eval true" [nix-instantiate --option pure-eval true --eval --strict -E $pure_probe]]
    ["nix-instantiate --pure-eval refuses a local file" [nix-instantiate --pure-eval --eval -E "builtins.readFile /etc/hostname"]]
  ] {
    $ok = ($ok | append (cmp-case $case.label (run-both $farm ($case.args | first) ...($case.args | skip 1))))
  }
  $ok = ($ok | append (cmp-case "nix.conf system reaches builtins.currentSystem"
    (with-env {NIX_CONFIG: "system = aarch64-linux"} { run-both $farm nix-instantiate --eval -E "builtins.currentSystem" })))
  # NIX_USER_CONF_FILES set and empty means no user file, so the system file wins
  let cdir = $"($tmp)/confdir"
  mkdir $cdir $"($env.XDG_CONFIG_HOME)/nix"
  "system = from-system\n" | save -f $"($cdir)/nix.conf"
  "system = from-user\n" | save -f $"($env.XDG_CONFIG_HOME)/nix/nix.conf"
  for case in [{label: "empty", v: ""} {label: "unset", v: null}] {
    $ok = ($ok | append (cmp-case $"NIX_USER_CONF_FILES ($case.label)"
      (with-env {NIX_CONF_DIR: $cdir, NIX_USER_CONF_FILES: $case.v} {
        run-both $farm nix-instantiate --eval -E "builtins.currentSystem" })))
  }
  rm -f $"($env.XDG_CONFIG_HOME)/nix/nix.conf"
  # installables.cc:460 refuses `nix eval -f` when `pure-eval` was overridden:
  # a flag, a user file, $NIX_CONFIG or --option, but not the system nix.conf
  # NIX_CONF_DIR replaces /etc/nix, where the features the `nix` name needs live
  "experimental-features = nix-command flakes\npure-eval = true\n" | save -f $"($cdir)/nix.conf"
  "pure-eval = true\n" | save -f $"($cdir)/user.conf"
  for case in [
    {label: "the system nix.conf", env: {NIX_CONF_DIR: $cdir, NIX_USER_CONF_FILES: ""}, args: []}
    {label: "a user nix.conf", env: {NIX_USER_CONF_FILES: $"($cdir)/user.conf"}, args: []}
    {label: "NIX_CONFIG", env: {NIX_CONFIG: "pure-eval = true"}, args: []}
    {label: "--option", env: {}, args: [--option pure-eval true]}
    {label: "--impure", env: {}, args: [--impure]}
  ] {
    $ok = ($ok | append (cmp-case $"nix eval -f with pure-eval from ($case.label)"
      (with-env $case.env { run-both $farm nix eval ...$case.args -f $"($d)/multi.nix" outputs })))
  }
  # `-j` and `--cores` set a setting, so the value is checked as one; the
  # clustered `-IP=PATH` is iets's own spelling and Nix refuses it
  for args in [[-j 4294967296] [--cores 4294967296] [-j auto] [--cores 1K] [-j 2] [-Ifoo=/etc]] {
    $ok = ($ok | append (cmp-case $"instantiate ($args | str join ' ')"
      (run-both $farm nix-instantiate ...($args | each { into string }) --eval -E "1")))
  }
  # the classic name is nix-eval-jobs, which says nothing on stderr
  let nej = (^$"($farm)/nix-eval-jobs" --no-instantiate -E "{ }" | complete)
  $ok = ($ok | append (verdict ($nej.stderr | is-empty) "nix-eval-jobs prints no summary" $"stderr '($nej.stderr)'"))
  let own = (^$iets eval-jobs --no-instantiate -E "{ }" | complete)
  $ok = ($ok | append (verdict ($own.stderr | str contains "eval-jobs: ") "iets eval-jobs keeps its summary" $"stderr '($own.stderr)'"))
  # a value Nix refuses stops the run with Nix's own text and exit 1; the
  # unit prefix and the words `auto` and `relaxed` are accepted
  let err1 = {|s: string| $s | ansi strip | lines | where ($it | str starts-with "error:") | get -o 0 | default "" }
  for v in ["pure-eval = maybe" "show-trace = maybe" "sandbox = maybe" "eval-cache = 2"
            "connect-timeout = abc" "cores = -1" "cores = 4294967296" "max-jobs = abc"
            "cores = 1K" "min-free = 2G" "max-jobs = auto" "sandbox = relaxed"] {
    let r = (with-env {NIX_CONFIG: $v} { run-both $farm nix-instantiate --eval -E "1" })
    let same = (($r.ours.exit_code == $r.real.exit_code)
      and (($r.ours.stdout | into binary) == ($r.real.stdout | into binary))
      and ((do $err1 $r.ours.stderr) == (do $err1 $r.real.stderr)))
    $ok = ($ok | append (verdict $same $"nix.conf '($v)'"
      $"'($v)': exit ours=($r.ours.exit_code) real=($r.real.exit_code); ours '(do $err1 $r.ours.stderr)' real '(do $err1 $r.real.stderr)'"))
  }
  $ok = ($ok | append (cmp-case "nix eval --expr, a nested error"
    (run-both $farm nix eval --expr "{ a = throw \"boom\"; b = 1; }")))
  $ok = ($ok | append (cmp-case "nix eval --expr, repeated and empty values"
    (run-both $farm nix eval --expr "let a = { x = 1; }; in [ a a {} {} [] ]")))
  $ok = ($ok | append (cmp-case "nix eval --expr, functions"
    (run-both $farm nix eval --expr "{ f = x: x; p = builtins.add; q = builtins.add 1; }")))

  # --- symlinks below a store path the evaluation copied. Nix reads such a
  # path out of the store copy, where a link resolves inside the copy; iets
  # reads the local tree the copy stands for, so it must resolve on the text
  # of the path instead (docs/explanation/flakes-and-sources.md).
  let sl = $"($tmp)/symlinks"
  mkdir $"($sl)/sub"
  "hello\n" | save $"($sl)/a.txt"
  "inner\n" | save $"($sl)/sub/inner"
  ^ln -s a.txt $"($sl)/link-file"
  ^ln -s sub $"($sl)/link-dir"
  ^ln -s nope $"($sl)/dangling"
  # a store path with a reference, for the copy that must keep it
  let withref = (^nix eval --impure --raw --expr r#'builtins.toFile "b" "ref ${builtins.toFile "a" "aaa"}"'# | complete | get stdout | chomp)
  for case in [
    [label expr];
    ["readFile through a copy of the link itself" r#'builtins.readFile "${@SL@/link-file}"'#]
    ["readFile of a target the filter left out" r#'builtins.readFile "${builtins.filterSource (p: _: baseNameOf p != "a.txt") @SL@}/link-file"'#]
    ["readDir through a copy of the link itself" r#'builtins.readDir "${@SL@/link-dir}"'#]
    ["hashFile through a copy of the link itself" r#'builtins.hashFile "sha256" "${@SL@/link-file}"'#]
    ["readFile below a link in a copied tree" r#'builtins.readFile "${@SL@}/link-dir/inner"'#]
    ["readDir of a link in a copied tree" r#'builtins.readDir "${@SL@}/link-dir"'#]
    ["pathExists below a link in a copied tree" r#'builtins.pathExists "${@SL@}/link-dir/inner"'#]
    ["path of a link in a copied tree" r#'builtins.path { path = "${@SL@}/link-dir"; }'#]
    ["path of a link, local and copied agree" r#'builtins.path { path = @SL@/link-dir; }'#]
    ["path of a dangling link" r#'builtins.path { path = @SL@/dangling; }'#]
    ["path of a dangling link in a copied tree" r#'builtins.path { path = "${@SL@}/dangling"; }'#]
    ["storePath below a link in a copied tree" r#'builtins.storePath "${@SL@}/link-dir/inner"'#]
    ["filterSource sees the store path of an unfiltered copy" r#'builtins.filterSource (p: _: builtins.match "/nix/store/.*" p != null) "${@SL@/sub}"'#]
    ["path keeps the references of a store path" r#'builtins.path { path = "${builtins.storePath @REF@}"; }'#]
    ["path flat refuses a store path with references" r#'builtins.path { path = "${builtins.storePath @REF@}"; recursive = false; }'#]
  ] {
    let e = ($case.expr | str replace -a "@SL@" $sl | str replace -a "@REF@" $withref)
    $ok = ($ok | append (cmp-case $"symlink: ($case.label)" (run-both $farm nix eval --impure --expr $e)))
  }
  $ok = ($ok | append (cmp-case "nix eval -f FILE ATTR"
    (run-both $farm nix eval -f $"($d)/multi.nix" outputs)))
  $ok = ($ok | append (cmp-case "nix eval --json -f FILE ATTR"
    (run-both $farm nix eval --json -f $"($d)/multi.nix" outputs)))

  # --- a file whose top level is a function: the attribute path auto-calls it
  # at every step, in the native names too (docs/reference/plumbing.md)
  let fun = $"($d)/fun.nix"
  '{ a ? 1, b ? 2 }: { sum = a + b; name = "n-${toString a}"; }' | save -f $fun
  let ell = $"($d)/ellipsis.nix"
  '{ a ? 1, ... }@args: { sum = a; name = builtins.toJSON (builtins.attrNames args); }' | save -f $ell
  for case in [
    {label: "defaults", args: [], attr: sum}
    {label: "--arg", args: [--arg a 5], attr: sum}
    {label: "--argstr", args: [--argstr a x], attr: name}
  ] {
    $ok = ($ok | append (cmp-case $"auto-call a function file, ($case.label): nix-instantiate"
      (run-both $farm nix-instantiate --eval ...$case.args $fun -A $case.attr)))
    $ok = ($ok | append (cmp-case $"auto-call a function file, ($case.label): iets eval"
      {ours: (^$iets eval ...$case.args $fun -A $case.attr | complete),
       real: (^nix eval ...$case.args -f $fun $case.attr | complete)}))
    # formals with `...` are handed every argument, named or not
    $ok = ($ok | append (cmp-case $"auto-call a '...' function file, ($case.label): nix-instantiate"
      (run-both $farm nix-instantiate --eval ...$case.args --arg extra 9 $ell -A $case.attr)))
    $ok = ($ok | append (cmp-case $"auto-call a '...' function file, ($case.label): iets eval"
      {ours: (^$iets eval ...$case.args --arg extra 9 $ell -A $case.attr | complete),
       real: (^nix eval ...$case.args --arg extra 9 -f $ell $case.attr | complete)}))
  }
  # a required formal nobody supplied: all three fail the same way
  let na = $"($d)/needs-arg.nix"
  '{ a, b ? 2 }: { sum = a + b; }' | save -f $na
  let first = {|s: string| $s | ansi strip | lines | where ($it | str starts-with "error:") | get -o 0 | default "" }
  let na_real = (^nix eval -f $na sum | complete)
  let na_eval = (^$iets eval $na -A sum | complete)
  let na_inst = (^$"($farm)/nix-instantiate" --eval $na -A sum | complete)
  let na_lines = [(do $first $na_real.stderr) (do $first $na_eval.stderr) (do $first $na_inst.stderr)]
  $ok = ($ok | append (verdict
    (([$na_real.exit_code $na_eval.exit_code $na_inst.exit_code] | all { $in == 1 })
      and ($na_lines | uniq | length) == 1 and ($na_lines | first) != "")
    "a required --arg that is missing: the same first error line everywhere"
    $"exit real=($na_real.exit_code) eval=($na_eval.exit_code) inst=($na_inst.exit_code), lines ($na_lines | to json -r)"))

  # --- nix search: a package set with nested recurseForDerivations sets, an
  # attribute that throws, a bad `type` and a description with a newline.
  r#'let
  mk = { pname, version ? "", description ? null }:
    derivation {
      name = if version == "" then pname else "${pname}-${version}";
      system = builtins.currentSystem;
      builder = "/bin/sh";
      args = [ "-c" "echo hi > $out" ];
    } // (if description == null then { } else { meta = { inherit description; }; });
in
{
  legacyPackages.${builtins.currentSystem} = {
    hello = mk { pname = "hello"; version = "2.12"; description = "A program that produces a familiar, friendly greeting"; };
    goodbye = mk { pname = "goodbye"; version = "1.0"; description = "Says goodbye\nacross two lines"; };
    plain = mk { pname = "plain"; };
    broken = throw "this attribute is broken";
    brokenSet = { recurseForDerivations = true; inner = throw "deep throw"; ok = mk { pname = "survivor"; version = "1"; description = "Found after a throw"; }; };
    notASet = 42;
    badType = { type = "derivation"; name = 7; };
    nested = {
      recurseForDerivations = true;
      tool = mk { pname = "nested-tool"; version = "0.1"; description = "A nested tool"; };
      deeper = { recurseForDerivations = true; deepest = mk { pname = "deepest"; version = "3"; description = "Deep package"; }; };
      unmarked = { hidden = mk { pname = "hidden"; version = "9"; description = "Not reachable"; }; };
    };
    metaNotASet = mk { pname = "metaless"; version = "2"; } // { meta = 5; };
    quotes = mk { pname = "quotes"; version = "1"; description = "Uses \"quotes\", a tab\tand a backslash \\"; };
    unicode = mk { pname = "unicode"; version = "1"; description = "Ünïcödé — em dash and ▲"; };
  };
  packages.${builtins.currentSystem}.direct = mk { pname = "direct"; version = "4"; description = "Under packages"; };
}
'# | save -f $"($d)/pkgs.nix"
  for case in [
    {label: "nix search, every package", args: ["^"]}
    {label: "nix search --json", args: ["^" --json]}
    {label: "nix search --json --pretty", args: ["^" --json --pretty]}
    {label: "nix search, two regexes are ANDed", args: [deep est]}
    {label: "nix search --exclude", args: [e -e good]}
    {label: "nix search is case insensitive", args: [HELLO]}
    {label: "nix search, a bracket expression", args: ['[Ff]riendly']}
    {label: "nix search, an alternation", args: ['hell|good']}
    {label: "nix search, no results", args: [zzz]}
    {label: "nix search --json, no results", args: [zzz --json]}
  ] {
    $ok = ($ok | append (cmp-case $case.label
      (run-both $farm nix search -f $"($d)/pkgs.nix" "" ...$case.args)))
  }
  # over the pinned nixpkgs: the walk that costs, under `legacyPackages` so
  # that both sides skip the attributes that throw.
  let npexpr = $"{ legacyPackages.${builtins.currentSystem} = import ($np) {}; }"
  for re in ["^hello$" ripgrep] {
    $ok = ($ok | append (cmp-case $"nix search nixpkgs --json ($re)"
      (run-both $farm nix search --impure --expr $npexpr "" $re --json)))
  }

  # --- --log-format internal-json: the same error record and exit code
  "let\n  x = 1;\nin\n  y + x\n" | save -f $"($d)/undef.nix"
  let json = [--log-format internal-json]
  $ok = ($ok | append (cmp-record "internal-json: undefined variable in a file" undefined-var
    (run-both $farm nix-instantiate ...$json --eval $"($d)/undef.nix")))
  $ok = ($ok | append (cmp-record "internal-json: missing attribute" missing-attr
    (run-both $farm nix-instantiate ...$json --eval -E "{ a = 1; }.b")))
  $ok = ($ok | append (cmp-record "internal-json: syntax error" parse
    (run-both $farm nix-instantiate ...$json --eval -E "1 +")))
  $ok = ($ok | append (cmp-record "internal-json: nix eval, missing attribute" missing-attr
    (run-both $farm nix eval ...$json --expr "{ a = 1; }.b")))
  # A daemon client nix exits 1 on any failed build, and so do the classic
  # names; `iets build` exits with the build status (docs/reference/errors.md).
  let fail = 'derivation { name = "classic-e2e-fail"; system = "x86_64-linux"; builder = "/bin/sh"; args = [ "-c" "echo oops; exit 3" ]; }'
  $ok = ($ok | append (cmp-record "internal-json: a failed build" build-failed
    (run-both $farm nix-build ...$json --no-out-link -E $fail)))
  let fod = 'derivation { name = "classic-e2e-fod"; system = "x86_64-linux"; builder = "/bin/sh"; args = [ "-c" "echo hi > $out" ]; outputHashMode = "flat"; outputHash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="; }'
  $ok = ($ok | append (cmp-record "internal-json: a hash mismatch" hash-mismatch
    (run-both $farm nix-build ...$json --no-out-link -E $fod)))
  let bf = (^$iets build -E $fail | complete)
  $ok = ($ok | append (verdict ($bf.exit_code == 100) "iets build: a failed build exits 100" $"exit ($bf.exit_code)"))
  # a successful build's block ends with its ✔: nothing follows it
  let okdrv = ('derivation { name = "classic-e2e-order"; system = "x86_64-linux"; builder = "/bin/sh"; args = [ "-c" "echo from-the-builder; echo x > $out" ]; salt = "SALT"; }' | str replace SALT (random uuid))
  let ob = (^$iets build -E $okdrv | complete)
  let olines = ($ob.stderr | ansi strip | lines)
  let at = {|pat: string| $olines | enumerate | where {|r| $r.item | str contains $pat } | get -o 0.index }
  let ilog = (do $at "from-the-builder")
  let iok = (do $at "✔")
  $ok = ($ok | append (verdict ($ob.exit_code == 0 and $ilog != null and $iok != null and $ilog < $iok)
    "iets build: the ✔ is the last line of a successful block" $"exit ($ob.exit_code), stderr ($ob.stderr)"))
  let hm = (^$iets build ...$json -E $fod | complete)
  $ok = ($ok | append (verdict ($hm.exit_code == 102 and ($hm.stderr | str contains '"hint":"set the expected hash to '))
    "iets build: a hash mismatch exits 102 and names the hash" $"exit ($hm.exit_code), stderr '($hm.stderr | str substring 0..400)'"))
  # every record of the run, activity records included, has the one sid
  let hm_recs = ($hm.stderr | lines | where ($it | str starts-with "@nix ") | each { str substring 5.. | from json })
  let hm_sids = ($hm_recs | get -o sid | uniq)
  $ok = ($ok | append (verdict (($hm_recs | where action == start | length) > 0 and ($hm_sids | length) == 1 and ($hm_recs | all { $in.sid? != null }))
    "iets build: every record, activities too, has one sid" $"sids ($hm_sids | to json -r)"))
  # fix-hashes: the fake hash in the file becomes the one the build got, and the rebuild succeeds
  mkdir $"($d)/fix"
  $"{ pkg = ($fod); }\n" | save -f $"($d)/fix/default.nix"
  let fx = (do { cd $"($d)/fix"; ^$iets --iets-option fix-hashes always build ./default.nix -A pkg | complete })
  let fixed = (open --raw $"($d)/fix/default.nix")
  $ok = ($ok | append (verdict ($fx.exit_code == 0 and not ($fixed | str contains "AAAAAAAA") and ($fx.stderr | str contains "fixed: "))
    "iets build with fix-hashes: rewrites the hash and builds" $"exit ($fx.exit_code), file '($fixed)', stderr '($fx.stderr | str substring 0..400)'"))
  # the default asks, and with no terminal nobody can answer: the file stays
  $"{ pkg = ($fod); }\n" | save -f $"($d)/fix/default.nix"
  let fa = (do { cd $"($d)/fix"; ^$iets build ./default.nix -A pkg | complete })
  $ok = ($ok | append (verdict ($fa.exit_code == 102 and (open --raw $"($d)/fix/default.nix" | str contains "AAAAAAAA"))
    "iets build without a terminal: fix-hashes asks nobody, the file stays" $"exit ($fa.exit_code)"))
  # a fixed-output derivation with an input derivation is built as a resolved
  # copy, whose mismatch the daemon reports as a line, not as the build's result
  let fodin = ('derivation { name = "classic-e2e-fod-in"; system = "x86_64-linux"; builder = "/bin/sh"; args = [ "-c" "echo hi > $out" ]; dep = derivation { name = "classic-e2e-fod-dep"; system = "x86_64-linux"; builder = "/bin/sh"; args = [ "-c" "echo SALT > $out" ]; }; outputHashMode = "flat"; outputHash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="; }' | str replace SALT (random uuid))
  $"{ pkg = ($fodin); }\n" | save -f $"($d)/fix/default.nix"
  let fr = (do { cd $"($d)/fix"; ^$iets --iets-option fix-hashes always build ./default.nix -A pkg | complete })
  let frfile = (open --raw $"($d)/fix/default.nix")
  $ok = ($ok | append (verdict ($fr.exit_code == 0 and not ($frfile | str contains "AAAAAAAA") and ($fr.stderr | str contains "fixed: "))
    "iets build with fix-hashes: a resolved derivation's mismatch is fixed too" $"exit ($fr.exit_code), file '($frfile)', stderr '($fr.stderr | str substring 0..600)'"))

  # --- activity records: a build's stream has the shape of Nix's, and nom reads it
  let act = {|| 'derivation { name = "classic-e2e-act"; system = "x86_64-linux"; builder = "/bin/sh"; args = [ "-c" "echo top; echo $0 > $out" (derivation { name = "classic-e2e-act-dep"; system = "x86_64-linux"; builder = "/bin/sh"; args = [ "-c" "echo dep; echo x > $out" ]; salt = "SALT"; }) ]; }' | str replace SALT (random uuid) }
  mut streams = []
  for cmd in [[nix-build --no-out-link -E] [nix build --no-link --expr]] {
    let run = {|bin| ^$bin --log-format internal-json ...($cmd | skip 1) (do $act) | complete }
    let o = (do $run $"($farm)/($cmd.0)")
    let r = (do $run $cmd.0)
    let a = (act-shape $r.stderr)
    let b = (act-shape $o.stderr)
    $ok = ($ok | append (verdict ($o.exit_code == 0 and $r.exit_code == 0 and $a == $b and ($a.order | length) > 0)
      $"internal-json: ($cmd | take 2 | str join ' ') activity records match Nix's" $"exit ours=($o.exit_code) real=($r.exit_code)\n  real: ($a | to json -r)\n  ours: ($b | to json -r)"))
    $streams = ($streams | append $o.stderr)
  }
  let nom = (^nix build --no-link --print-out-paths -f $np nix-output-monitor | complete)
  let seen = if $nom.exit_code == 0 {
    $streams | each {|s| $s | ^$"($nom.stdout | chomp)/bin/nom" --json | complete | get stderr | ansi strip }
  } else { [$"cannot build nom: ($nom.stderr | str substring 0..400)"] }
  for s in $seen {
    $ok = ($ok | append (verdict ((not ($s | str contains "nix-output-monitor error")) and ($s | str contains "classic-e2e-act-dep") and ($s | str contains "Finished at"))
      "nom --json reads iets's activity records" $"nom said '($s | str substring (-600)..)'"))
  }

  # --- the roots a build keeps. `--no-out-link` and `--no-link` ask for no
  # symlink; nix then lets the output go, and iets keeps a root in its roots
  # store instead (docs/reference/divergences.md) — a side effect on the
  # daemon, so stdout and the exit code still match nix's byte for byte.
  # Native `iets build` roots every path it prints, for the same reason: the
  # caller uses those paths after the run.
  let brdrv = ('derivation { name = "classic-e2e-noout"; system = "@SYS@"; builder = "/bin/sh"; args = [ "-c" "echo x > $out" ]; salt = "SALT"; }' | str replace SALT (random uuid) | str replace -a "@SYS@" (current-system))
  let brdir = $"($tmp)/noout"
  mkdir $brdir
  cd $brdir
  for case in [
    {label: "nix-build --no-out-link -E", name: "nix-build", args: [--no-out-link -E]}
    {label: "nix build --no-link --print-out-paths", name: "nix", args: [build --no-link --print-out-paths --expr]}
  ] {
    let cache = $"($tmp)/broots-($case.name)"
    let r = (with-env {IETS_CACHE_DIR: $cache} { run-both $farm $case.name ...$case.args $brdrv })
    $ok = ($ok | append (cmp-case $case.label $r))
    let links = (try { ls $"($cache)/roots" | get name | sort } catch { [] })
    let link = ($links | get -o 0 | default "")
    let target = (if ($link | is-not-empty) { ^readlink $link | chomp } else { "" })
    let held = (if ($target | is-not-empty) { (^nix-store --query --roots $target | complete).stdout | str contains $link } else { false })
    $ok = ($ok | append (verdict (($links | length) == 1 and $target == ($r.ours.stdout | chomp) and $held and (not ("result" | path exists)))
      $"($case.label) keeps a root instead of nothing" $"($links | length) links, '($link)' -> '($target)', held ($held), printed ($r.ours.stdout | chomp)"))
  }
  let broff = $"($tmp)/broots-off"
  let nbo = (with-env {IETS_CACHE_DIR: $broff} { ^$"($farm)/nix-build" --iets-option build-gc-root false --no-out-link -E $brdrv | complete })
  $ok = ($ok | append (verdict ($nbo.exit_code == 0 and (not ($"($broff)/roots" | path exists)))
    "build-gc-root = false keeps no root, as in nix" $"exit ($nbo.exit_code), stderr ($nbo.stderr | str substring 0..200)"))
  let nvcache = $"($tmp)/broots-native"
  let nv = (with-env {IETS_CACHE_DIR: $nvcache} { ^$iets build -E $brdrv | complete })
  let nvlinks = (try { ls $"($nvcache)/roots" | get name } catch { [] })
  let nvtarget = (if (($nvlinks | length) == 1) { ^readlink ($nvlinks | first) | chomp } else { "" })
  $ok = ($ok | append (verdict ($nv.exit_code == 0 and $nvtarget == ($nv.stdout | chomp))
    "iets build roots the path it printed" $"exit ($nv.exit_code), ($nvlinks | length) links -> '($nvtarget)', printed ($nv.stdout | chomp)"))
  cd $tmp

  # --- iets build --dry-run: nix build --dry-run's text, and the same sets as JSON
  let dry = (do $act)
  let dr = (^nix build --dry-run --expr $dry | complete)
  let dj = (^$iets build --dry-run --json -E $dry | complete)
  let plan = ($dj.stdout | from json)
  $ok = ($ok | append (verdict ($dj.exit_code == 0 and $dj.stderr == $dr.stderr and ($plan.willBuild | length) == 2 and ($dr.stderr | str contains ($plan.willBuild | str join "\n  ")))
    "iets build --dry-run: nix build --dry-run's text, and the plan as JSON" $"exit ($dj.exit_code)\n  real: ($dr.stderr)\n  ours: ($dj.stderr)\n  json: ($dj.stdout)"))

  # --- json-log-path: stderr stays text, the file gets the same records unprefixed
  let jlog = {|f| {stderr: (open --raw $f | lines | each {|l| $"@nix ($l)" } | str join "\n")} }
  let jo = (^$"($farm)/nix" eval --option json-log-path $"($d)/jlog-ours" --expr "{ a = 1; }.b" | complete)
  let jr = (^$"($env.IETS_NIX_BIN_DIR)/nix" eval --option json-log-path $"($d)/jlog-real" --expr "{ a = 1; }.b" | complete)
  $ok = ($ok | append (cmp-record "json-log-path: the file holds the error record" missing-attr
    {ours: ({exit_code: $jo.exit_code} | merge (do $jlog $"($d)/jlog-ours")),
     real: ({exit_code: $jr.exit_code} | merge (do $jlog $"($d)/jlog-real"))}))
  $ok = ($ok | append (verdict ($jo.stderr | str starts-with "error: attribute 'b' missing")
    "json-log-path: stderr stays text" $"stderr '($jo.stderr | str substring 0..200)'"))
  let jb = (^$iets build --json-log-path $"($d)/jlog-build" -E (do $act) | complete)
  let jrecs = (open --raw $"($d)/jlog-build" | lines | each { from json })
  $ok = ($ok | append (verdict ($jb.exit_code == 0 and ($jrecs | where {|r| $r.action == "start" and $r.type? == 105 } | length) == 2 and not ($jb.stderr | str contains "@nix"))
    "json-log-path: the file gets a build's activity records, stderr stays text" $"exit ($jb.exit_code), stderr '($jb.stderr | str substring 0..300)'"))

  # --- a flag or a setting with no effect: one warning that gives the
  # reason, where Nix prints nothing; stdout and the exit code stay Nix's
  # (docs/reference/divergences.md)
  let recs = {|stderr, code| $stderr | lines | where ($it | str starts-with "@nix ") | each { str substring 5.. | from json } | where code? == $code }
  let once = {|stderr, text| ($stderr | lines | where ($it | str contains $text) | length) == 1 }
  let vb = (run-both $farm nix-instantiate --eval -v --verbose -E "1 + 1")
  $ok = ($ok | append ((cmp-case "nix-instantiate -v --verbose" $vb) and (verdict (do $once $vb.ours.stderr "warning: '--verbose' is ignored: iets has one log level")
    "-v is one warning with the reason" $"stderr '($vb.ours.stderr)'")))
  let ncdir = $"($tmp)/flake-nixconfig"
  mkdir $ncdir
  cp $"($fdir)/flake.lock" $ncdir
  "{\n  nixConfig.extra-substituters = [ \"https://cache.invalid\" ];\n  outputs = { self }: { x = 1; };\n}\n" | save $"($ncdir)/flake.nix"
  let ac = (run-both $farm nix eval --accept-flake-config --eval-store auto --eval-store auto $"path:($ncdir)#x")
  $ok = ($ok | append ((cmp-case "nix eval --accept-flake-config --eval-store auto, a flake with a nixConfig" $ac)
    and (verdict ((do $once $ac.ours.stderr "'--accept-flake-config' is ignored: iets never applies a flake's nixConfig")
        and (do $once $ac.ours.stderr "'--eval-store' is ignored: iets evaluates in memory")
        and not ($ac.ours.stderr | str contains "falling back"))
      "--accept-flake-config and --eval-store auto are one warning each, with the reason" $"stderr '($ac.ours.stderr)'")))
  let aj = (^$"($farm)/nix" eval --log-format internal-json --accept-flake-config --expr "1" | complete)
  let ajr = (do $recs $aj.stderr "ignored-flag")
  $ok = ($ok | append (verdict ($aj.stdout == "1\n" and ($ajr | length) == 1 and $ajr.0.level == 1 and ($ajr.0.raw_msg | str contains "nixConfig"))
    "an ignored flag is a level-1 ignored-flag record" $"stderr '($aj.stderr)'"))
  let jlf = $"($tmp)/ignored.jsonl"
  ^$"($farm)/nix" eval --option json-log-path $jlf --eval-store auto --expr "1" | complete | ignore
  $ok = ($ok | append (verdict ((open --raw $jlf | lines | each { from json } | where code? == "ignored-flag" | length) == 1)
    "json-log-path gets the ignored-flag record" (open --raw $jlf)))
  # a nix.conf written for a fork of Nix is a file of unknown settings
  let fconf = $"($d)/fork.conf"
  "lazy-trees = true\neval-cores = 0\n" | save -f $fconf
  let fk = (with-env {NIX_USER_CONF_FILES: $fconf} { run-both $farm nix-instantiate --eval -E "1 + 1" })
  $ok = ($ok | append ((cmp-case "a nix.conf with the settings of a fork changes nothing" $fk)
    and (verdict ($fk.ours.stderr == $fk.real.stderr and (do $once $fk.real.stderr "unknown setting 'lazy-trees'"))
      "each of them is Nix's own unknown-setting warning, once" $"ours '($fk.ours.stderr)' real '($fk.real.stderr)'")))
  let fj = (with-env {NIX_USER_CONF_FILES: $fconf} { ^$iets eval --log-format internal-json --option eval-cores 4 -E "1" | complete })
  let fjr = (do $recs $fj.stderr "unknown-setting")
  $ok = ($ok | append (verdict ($fj.stdout == "1\n" and ($fjr | get raw_msg) == ["unknown setting 'eval-cores'" "unknown setting 'lazy-trees'"] and ($fjr | all { $in.level == 1 }))
    "an unknown setting is a level-1 unknown-setting record, once per name" $"stderr '($fj.stderr)'"))
  let uk = (run-both $farm nix-instantiate --eval -E "1" --option no-such-setting 1 --option extra-no-such 2)
  $ok = ($ok | append ((cmp-case "an unknown setting" $uk) and (verdict ($uk.ours.stderr == $uk.real.stderr and ($uk.real.stderr | str contains "unknown setting 'no-such-setting'"))
    "an unknown setting gets Nix's own warning" $"ours '($uk.ours.stderr)' real '($uk.real.stderr)'")))
  let afc = (with-env {NIX_CONFIG: "accept-flake-config = true\nlog-lines = 7\nwarn-dirty = false\nmax-call-depth = 10000"} { run-both $farm nix eval --expr "1" })
  $ok = ($ok | append ((cmp-case "accept-flake-config = true in NIX_CONFIG" $afc)
    and (verdict ((do $once $afc.ours.stderr "setting 'accept-flake-config' is ignored:") and (do $once $afc.ours.stderr "setting 'log-lines' is ignored:")
        and ($afc.ours.stderr | lines | length) == 2)
      "a client setting iets has nothing for is one warning; a default and one that holds are none" $"stderr '($afc.ours.stderr)'")))
  let re = [nix-instantiate --eval -E "builtins.readFile /etc/hostname" --option restrict-eval "true"]
  $ok = ($ok | append ((cmp-case "restrict-eval is left to the real nix" (run-both $farm ($re | first) ...($re | skip 1)))
    and (verdict ((fallback-reason $farm $re) | str contains "the setting 'restrict-eval' is not implemented")
      "restrict-eval falls back" (fallback-reason $farm $re))))
  let ns = (^$iets eval --log-format internal-json --option build-use-sandbox false --option eval-cache false -E "1" | complete)
  let nsr = (do $recs $ns.stderr "ignored-setting")
  $ok = ($ok | append (verdict ($ns.stdout == "1\n" and ($nsr | length) == 1 and ($nsr.0.raw_msg | str starts-with "setting 'sandbox' is ignored: iets's own commands send a daemon no settings"))
    "iets eval --option sandbox: one ignored-setting record" $"stderr '($ns.stderr)'"))

  # --- the daemon gets the store settings of the command line, as from Nix
  let slow = $"($tmp)/slow.nix"
  "derivation {\n  name = \"slow-${toString builtins.currentTime}\";\n  system = builtins.currentSystem;\n  builder = \"/bin/sh\";\n  args = [ \"-c\" \"while :; do :; done\" ];\n}\n" | save $slow
  let to = (run-both $farm nix-build $slow --no-out-link --option timeout 2)
  $ok = ($ok | append ((cmp-case "nix-build --option timeout 2" $to) and (verdict ($to.ours.exit_code == 1 and ($to.ours.stderr | str contains "timed out after 2 seconds"))
    "--option timeout reaches the daemon" $"stderr '($to.ours.stderr | str substring 0..400)'")))
  # nix develop sends them too: the shell's input times out on both sides
  let sdir = $"($tmp)/slow-shell"
  mkdir $sdir
  cp $"($fdir)/flake.lock" $sdir
  "{\n  outputs = { self }: {\n    devShells.x86_64-linux.default = derivation {\n      name = \"slow-shell\";\n      system = \"x86_64-linux\";\n      builder = \"/bin/bash\";\n      slow = derivation {\n        name = \"slow-input-${toString builtins.currentTime}\";\n        system = \"x86_64-linux\";\n        builder = \"/bin/sh\";\n        args = [ \"-c\" \"while :; do :; done\" ];\n      };\n    };\n  };\n}\n" | save $"($sdir)/flake.nix"
  let sd = (run-both $farm nix develop --impure $"path:($sdir)" --option timeout 2 -c "true")
  $ok = ($ok | append ((cmp-case "nix develop --option timeout 2" $sd) and (verdict ($sd.ours.exit_code == 1 and ($sd.ours.stderr | ansi strip | str contains "timed out after 2 seconds"))
    "nix develop sends its build settings to the daemon" $"stderr '($sd.ours.stderr | str substring 0..400)'")))

  # --- the nix-path setting, which NIX_PATH replaces, even an empty one
  let npd = $"($tmp)/nixpath"
  mkdir $npd
  "\"from nix-path\"\n" | save $"($npd)/x.nix"
  let np_run = {|who, extra| ^env -u NIX_PATH ...$extra $"NIX_CONFIG=nix-path = np=($npd)" $who --eval -E "import <np/x.nix>" | complete }
  for c in [[label extra]; ["the nix-path setting" []] ["an empty NIX_PATH replaces the nix-path setting" ["NIX_PATH="]]] {
    $ok = ($ok | append (cmp-case $c.label {ours: (do $np_run $"($farm)/nix-instantiate" $c.extra), real: (do $np_run nix-instantiate $c.extra)}))
  }


  # --- nix build: the out-link prefix, the printed paths and the link target
  mut got = {}
  for who in [real ours] {
    let wd = $"($tmp)/nixbuild-($who)"
    mkdir $wd
    cd $wd
    let cmd = if $who == "real" { "nix" } else { $"($farm)/nix" }
    let r = (^$cmd build $"($fref)#pkg" --print-out-paths -o result-x | complete)
    let link = (if ("result-x" | path exists) { ^readlink result-x | chomp } else { "MISSING" })
    $got = ($got | insert $who {out: $r.stdout, code: $r.exit_code, link: $link, err: $r.stderr})
  }
  let same = (($got.ours.out | into binary) == ($got.real.out | into binary)) and ($got.ours.code == $got.real.code) and ($got.ours.link == $got.real.link)
  if not $same {
    print $"  ours: code=($got.ours.code) link=($got.ours.link) out=($got.ours.out)"
    print $"  real: code=($got.real.code) link=($got.real.link) out=($got.real.out)"
    print $"  ours stderr: ($got.ours.err | str substring 0..400)"
  }
  $ok = ($ok | append (verdict $same "nix build .#pkg --print-out-paths -o result-x" "see above"))

  # `nix print-dev-env` is `iets print-dev-env`, so only the dispatch is new.
  let pde_nix = (^$"($farm)/nix" print-dev-env $"($fref)#pkg" | complete)
  let pde_own = (^(^readlink -f $"($farm)/nix" | chomp) print-dev-env $"($fref)#pkg" | complete)
  $ok = ($ok | append (verdict (($pde_nix.stdout == $pde_own.stdout) and ($pde_nix.exit_code == $pde_own.exit_code))
    "nix print-dev-env is iets print-dev-env" $"exit ($pde_nix.exit_code) vs ($pde_own.exit_code)"))

  # --- nix derivation show: the derivation JSON of Nix 2.34.8, byte for byte.
  # Nothing here is built; the fixed-output case carries a hash no build matches.
  let fodshow = $"($tmp)/show-fod.nix"
  r#'derivation {
  name = "classic-e2e-show-fod";
  system = "x86_64-linux";
  builder = "/bin/sh";
  args = [ "-c" "echo x > $out" ];
  outputHashMode = "recursive";
  outputHashAlgo = "sha1";
  outputHash = "0000000000000000000000000000000000000000";
}
'# | save -f $fodshow
  let depshow = $"($tmp)/show-dep.nix"
  r#'let
  dep = derivation {
    name = "classic-e2e-show-dep";
    system = "x86_64-linux";
    outputs = [ "out" "dev" ];
    builder = "/bin/sh";
    args = [ "-c" "echo a > $out; echo b > $dev" ];
  };
in {
  top = derivation {
    name = "classic-e2e-show-top";
    system = "x86_64-linux";
    builder = "/bin/sh";
    args = [ "-c" "true" ];
    d = "${dep.dev}";
    t = builtins.toFile "note" "hello";
    __structuredAttrs = true;
  };
  inherit dep;
}
'# | save -f $depshow
  for case in [
    {label: "derivation show a custom drv", args: [-f $"($d)/default.nix"]}
    {label: "derivation show a multi-output drv", args: [-f $"($d)/multi.nix"]}
    {label: "derivation show a fixed-output drv", args: [-f $fodshow]}
    {label: "derivation show structured attrs and an input drv", args: [-f $depshow top]}
    {label: "derivation show two installables", args: [-f $depshow top dep]}
    {label: "derivation show --pretty", args: [--pretty -f $"($d)/multi.nix"]}
    {label: "derivation show --no-pretty", args: [--no-pretty -f $"($d)/multi.nix"]}
    {label: "derivation show .#pkg", args: [$"($fref)#pkg"]}
    {label: "derivation show -r over a real closure", args: [-r -f $depshow top]}
    {label: "derivation show -r over nixpkgs hello.src", args: [-r -I $"nixpkgs=($np)" -f $np hello.src]}
    {label: "derivation show structuredAttrs floats", args: [-f $"($d)/floats.nix"]}
    {label: "derivation show structuredAttrs floats --pretty", args: [--pretty -f $"($d)/floats.nix"]}
  ] {
    $ok = ($ok | append (cmp-case $case.label (run-both $farm nix derivation show ...$case.args)))
  }
  let miss = (run-both $farm nix derivation show -f $depshow nope)
  $ok = ($ok | append (cmp-case "derivation show an installable that does not exist" $miss))
  # only the first line: nix adds a "Did you mean" line that iets has nowhere
  # (docs/reference/classic.md).
  $ok = ($ok | append (verdict (($miss.ours.stderr | lines | get -o 0) == ($miss.real.stderr | lines | get -o 0))
    "derivation show an installable that does not exist: the same error line"
    $"ours '($miss.ours.stderr)' real '($miss.real.stderr)'"))
  # iets answers from the derivations the evaluation holds, so the store does
  # not gain the `.drv` the real nix registers (docs/reference/divergences.md).
  let saltshow = $"($tmp)/show-salt.nix"
  ('derivation { name = "classic-e2e-show-salt"; system = "x86_64-linux"; builder = "/bin/sh"; args = [ "-c" "true" ]; salt = "SALT"; }' | str replace SALT (random uuid)) | save -f $saltshow
  let so = (^$"($farm)/nix" derivation show -f $saltshow | complete)
  let sp = $"/nix/store/(($so.stdout | from json).derivations | columns | get -o 0 | default missing)"
  let before = ($sp | path exists)
  let sr = (^nix derivation show -f $saltshow | complete)
  $ok = ($ok | append (verdict ($so.stdout == $sr.stdout and not $before and ($sp | path exists))
    "derivation show does not instantiate, where the real nix does"
    $"($sp): ours left it ($before), real left it ($sp | path exists)"))
  for f in [[--stdin] [/nix/store/00000000000000000000000000000000-x.drv]] {
    let why = (fallback-reason $farm ([nix derivation show] | append $f))
    $ok = ($ok | append (verdict ($why | str contains "iets: falling back to nix:")
      $"derivation show ($f | str join ' ') falls back" $"stderr was '($why)'"))
  }

  # --- nix-shell: the derivation's inputs, then bash on nix-build.cc's rc file.
  # `<nixpkgs>` resolves to nixpkgs/shell.nix, which fetches a pinned tree, so
  # the real code path needs default.nix by name.
  cd $tmp
  let pkgs_nix = $"($np)/default.nix"
  $ok = ($ok | append (cmp-case "nix-shell nixpkgs -A hello --run 'echo $name'"
    (run-both $farm nix-shell -I $"nixpkgs=($np)" $pkgs_nix -A hello --run 'echo $name')))
  $ok = ($ok | append (cmp-env "nix-shell nixpkgs -A hello --run env"
    (run-both $farm nix-shell -I $"nixpkgs=($np)" $pkgs_nix -A hello --run env)))
  $ok = ($ok | append (cmp-env "nix-shell --pure --run env"
    (run-both $farm nix-shell -I $"nixpkgs=($np)" $pkgs_nix -A hello --pure --run env)))
  $ok = ($ok | append (cmp-case "nix-shell --pure --keep FOO"
    (with-env {FOO: "bar"} { run-both $farm nix-shell -I $"nixpkgs=($np)" $pkgs_nix -A hello --pure --keep FOO --run 'echo "[$FOO]"' })))
  # --command is interactive unless stdin is not a tty: an empty pipe settles it.
  let cmd_ours = ("" | ^$"($farm)/nix-shell" -I $"nixpkgs=($np)" $pkgs_nix -A hello --command 'echo $name' | complete)
  let cmd_real = ("" | ^nix-shell -I $"nixpkgs=($np)" $pkgs_nix -A hello --command 'echo $name' | complete)
  $ok = ($ok | append (cmp-case "nix-shell --command, stdin not a tty" {ours: $cmd_ours, real: $cmd_real}))

  # a plain derivation: shell.nix wins over default.nix, and passAsFile
  let sh = $"($tmp)/shellnix"
  mkdir $sh
  r#'derivation {
  name = "classic-e2e-shell";
  system = "x86_64-linux";
  builder = "/bin/sh";
  args = [ "-c" "echo hi > $out" ];
  MARK = "from-@WHICH@";
  passAsFile = "BIG";
  BIG = "big-value";
}
'# | str replace -a "@WHICH@" "shell-nix" | save $"($sh)/shell.nix"
  open --raw $"($sh)/shell.nix" | str replace -a "shell-nix" "default-nix" | save $"($sh)/default.nix"
  cd $sh
  $ok = ($ok | append (cmp-case "nix-shell with no argument prefers shell.nix"
    (run-both $farm nix-shell --run 'echo $MARK; cat $BIGPath')))
  $ok = ($ok | append (cmp-case "nix-shell --run 'exit 3' keeps the exit code"
    (run-both $farm nix-shell --run "exit 3")))
  cd $tmp
  $ok = ($ok | append (cmp-case "nix-shell with no shell.nix or default.nix"
    (run-both $farm nix-shell --run "true")))

  # --- nix-shell keeps what it realised under GC roots of its own. The inputs
  # are not one store path, so the roots store takes one link each. The
  # collection is simulated on a scratch chroot daemon; `nix collect-garbage`
  # never runs on this machine's store. `NIX_BUILD_SHELL` keeps `<nixpkgs>`
  # out of a fixture that has none.
  let sdir = $"($tmp)/shellroot"
  mkdir $sdir
  r#'let
  dep = derivation {
    name = "shellroot-dep";
    system = "@SYS@";
    builder = "/bin/sh";
    args = [ "-c" "echo building the dep >&2; echo @NONCE@ > $out" ];
  };
in derivation {
  name = "shellroot-shell";
  system = "@SYS@";
  builder = "/bin/sh";
  args = [ "-c" "echo hi > $out" ];
  DEP = dep;
}
'# | str replace -a "@SYS@" (current-system) | str replace -a "@NONCE@" $"(random int 0..999999)" | save -f $"($sdir)/shell.nix"
  # a short name: the daemon socket path has to fit in `sockaddr_un`
  let sstore = $"($tmp)/sr"
  mkdir $sstore
  let ssock = (scratch-daemon $sstore --option substituters "" --option builders "")
  let sscratch = $"local?root=($sstore)"
  let scache = $"($tmp)/shellroots"
  let senv = {IETS_CACHE_DIR: $scache, NIX_BUILD_SHELL: (^bash -c 'command -v bash' | chomp)}
  cd $sdir
  let s1 = (with-env $senv { ^$"($farm)/nix-shell" --option store $"unix://($ssock)" --run 'true' | complete })
  let slinks = (try { ls $"($scache)/roots" | get name | sort } catch { [] })
  let slink = ($slinks | get -o 0 | default "")
  let starget = (if ($slink | is-not-empty) { ^readlink $slink | chomp } else { "" })
  let sheld = (if ($starget | is-not-empty) { (^nix-store --store $sscratch --query --roots $starget | complete).stdout | str contains $slink } else { false })
  $ok = ($ok | append (verdict ($s1.exit_code == 0 and ($s1.stderr | str contains "building the dep") and ($slinks | length) == 1 and ($starget | str ends-with "-shellroot-dep") and $sheld)
    "nix-shell roots the input it realised" $"exit ($s1.exit_code), ($slinks | length) links, '($slink)' -> '($starget)', held: ($sheld)"))
  # the simulated collection keeps the rooted output, so the second entry
  # builds nothing
  let sgc = (^nix-store --store $sscratch --gc | complete)
  let skept = ($"($sstore)($starget)" | path exists)
  let s2 = (with-env $senv { ^$"($farm)/nix-shell" --option store $"unix://($ssock)" --run 'true' | complete })
  $ok = ($ok | append (verdict ($sgc.exit_code == 0 and $skept and $s2.exit_code == 0 and (not ($s2.stderr | str contains "building the dep")))
    "a collection leaves nix-shell's root, and the next entry builds nothing" $"kept ($skept), stderr ($s2.stderr | str substring 0..200)"))
  # `iets cache gc` walks the roots store and releases the link
  let scgc = (^$iets cache gc --ttl 0 --cache-dir $scache | complete)
  $ok = ($ok | append (verdict ($scgc.exit_code == 0 and ($scgc.stdout | str contains $"($scache)/roots: 1 of 1 entries") and (not ($slink | path exists)))
    "iets cache gc sweeps nix-shell's roots" $"exit ($scgc.exit_code), stdout ($scgc.stdout)"))
  # the switch, and the proof that the root is what held the output: with
  # `shell-gc-root` off, the same collection takes it and the next entry builds
  let soff = $"($tmp)/shellroots-off"
  let offenv = {IETS_CACHE_DIR: $soff, NIX_BUILD_SHELL: ($senv.NIX_BUILD_SHELL)}
  let s3 = (with-env $offenv { ^$"($farm)/nix-shell" --iets-option shell-gc-root false --option store $"unix://($ssock)" --run 'true' | complete })
  let off_roots = ($"($soff)/roots" | path exists)
  let sgc2 = (^nix-store --store $sscratch --gc | complete)
  let s4 = (with-env $offenv { ^$"($farm)/nix-shell" --iets-option shell-gc-root false --option store $"unix://($ssock)" --run 'true' | complete })
  $ok = ($ok | append (verdict ($s3.exit_code == 0 and (not $off_roots) and $sgc2.exit_code == 0 and ($s4.stderr | str contains "building the dep"))
    "shell-gc-root = false keeps no root, and the collection takes the output" $"roots store ($off_roots), stderr ($s4.stderr | str substring 0..200)"))
  cd $tmp

  # --- nix develop: the print-dev-env environment, then bash. This repository's
  # own flake is the only mkShell-shaped shell around (the fixture has no nixpkgs).
  let repo = ($env.FILE_PWD | path join ".." | path expand)
  $ok = ($ok | append (cmp-env "nix develop -c env"
    (run-both $farm nix develop $repo -c env)))
  $ok = ($ok | append (cmp-case "nix develop -c sh -c 'echo $name'"
    (run-both $farm nix develop $repo -c sh -c 'echo $name')))
  $ok = ($ok | append (cmp-env "nix develop --ignore-environment --keep HOME"
    (run-both $farm nix develop $repo -i -k HOME -c env)))
  $ok = ($ok | append (cmp-case "nix develop -u HOME"
    (run-both $farm nix develop $repo -u HOME -c sh -c 'echo "[$HOME]"')))

  # nix develop keeps the `-env` output under a GC root of its own: one link in
  # the roots store, keyed on the flake directory and the attribute path, which
  # a second entry replaces in place. The output references the whole shell, so
  # one link holds all of it.
  let devcache = $"($tmp)/devcache"
  let dev = (with-env {IETS_CACHE_DIR: $devcache} { ^$"($farm)/nix" develop $repo -c true | complete })
  let again = (with-env {IETS_CACHE_DIR: $devcache} { ^$"($farm)/nix" develop $repo -c true | complete })
  let dlinks = (try { ls $"($devcache)/roots" | get name | sort } catch { [] })
  let dlink = ($dlinks | get -o 0 | default "")
  let dtarget = (if ($dlink | is-not-empty) { ^readlink $dlink | chomp } else { "" })
  let dheld = (if ($dtarget | is-not-empty) { (^nix-store --query --roots $dtarget | complete).stdout | str contains $dlink } else { false })
  $ok = ($ok | append (verdict ($dev.exit_code == 0 and $again.exit_code == 0 and ($dlinks | length) == 1 and ($dtarget | str ends-with "-env") and $dheld)
    "nix develop keeps the shell's environment under a GC root" $"($dlinks | length) links, '($dlink)' -> '($dtarget)', held: ($dheld)"))
  # the switch: no link, and no roots store either
  let devoff = $"($tmp)/devcache-off"
  let dv = (with-env {IETS_CACHE_DIR: $devoff} { ^$"($farm)/nix" develop --iets-option shell-gc-root false $repo -c true | complete })
  $ok = ($ok | append (verdict ($dv.exit_code == 0 and (not ($"($devoff)/roots" | path exists)))
    "shell-gc-root = false keeps no root for nix develop" $"exit ($dv.exit_code), stderr ($dv.stderr | str substring 0..200)"))

  # --- nix fmt: the flake's formatter.<system>, built once and kept alive by
  # an out-link that is also a GC root. The formatter needs a `mkdir`, and the
  # build sandbox holds only the derivation's inputs, so nixpkgs comes in as a
  # `path:` input the real `nix flake lock` locks offline.
  let sys = (current-system)
  let fmtdir = $"($tmp)/fmtflake"
  let fmtcache = $"($tmp)/fmtcache"
  mkdir $fmtdir
  r##'{
  inputs.nixpkgs.url = "path:@NIXPKGS@";
  outputs = { self, nixpkgs }:
    let
      system = "@SYS@";
      cu = nixpkgs.legacyPackages.${system}.coreutils;
    in {
      formatter.${system} = derivation {
        name = "fmt-e2e-1.0";
        inherit system;
        builder = "/bin/sh";
        args = [ "-c" "${cu}/bin/mkdir -p $out/bin && echo '#!/bin/sh' > $out/bin/fmt-e2e && echo 'echo \"PRJ_ROOT=$PRJ_ROOT\"' >> $out/bin/fmt-e2e && echo 'echo \"args:$@\"' >> $out/bin/fmt-e2e && ${cu}/bin/chmod +x $out/bin/fmt-e2e" ];
      };
    };
}
'## | str replace -a "@NIXPKGS@" $np | str replace -a "@SYS@" $sys | save -f $"($fmtdir)/flake.nix"
  let flock = (do { cd $fmtdir; ^nix flake lock } | complete)
  if $flock.exit_code != 0 { print $"FAIL nix flake lock: ($flock.stderr)" }
  $ok = ($ok | append (verdict ($flock.exit_code == 0) "nix flake lock for the fmt fixture" $flock.stderr))
  # the out-link goes under this cache root, so the test can find it
  $env.IETS_CACHE_DIR = $fmtcache
  cd $fmtdir
  # `name` has a version and there is no `meta.mainProgram`, so the program is
  # names.cc:DrvName's `fmt-e2e`
  $ok = ($ok | append (cmp-case "nix fmt with no arguments" (run-both $farm nix fmt)))
  $ok = ($ok | append (cmp-case "nix fmt with paths"
    (run-both $farm nix fmt a.nix sub/b.nix)))
  let dd_ours = (^$"($farm)/nix" fmt -- --write x.nix | complete)
  let dd_real = (^nix fmt -- --write x.nix | complete)
  $ok = ($ok | append (cmp-case "nix fmt -- --write x.nix" {ours: $dd_ours, real: $dd_real}))
  # an edit invalidates the entry. Nix says nothing when its evaluation cache
  # misses, so neither does the classic name; json-log-path gets the record
  "\n# edited\n" | save --append $"($fmtdir)/flake.nix"
  let inv = (^$"($farm)/nix" fmt --option json-log-path $"($tmp)/fmt-jlog" | complete)
  let inv_recs = (try { open --raw $"($tmp)/fmt-jlog" | lines | each { from json } | where code? == "cache-invalidated" | length } catch { 0 })
  $ok = ($ok | append (verdict ($inv.exit_code == 0 and (not ($inv.stderr | str contains "re-evaluating")) and $inv_recs == 1)
    "nix fmt: an invalidation is silent on stderr and one record in json-log-path" $"($inv_recs) records, stderr ($inv.stderr | str substring 0..200)"))

  # the link is in the roots store, points into /nix/store, and the daemon
  # holds it: `nix-store --query --roots` names it
  let links = (try { ls $"($fmtcache)/roots" | get name } catch { [] })
  let link = ($links | get -o 0 | default "")
  let target = (if ($link | is-not-empty) { ^readlink $link | chomp } else { "" })
  let held = (if ($target | is-not-empty) { (^nix-store --query --roots $target | complete).stdout | str contains $link } else { false })
  $ok = ($ok | append (verdict (($links | length) == 1 and ($target | str starts-with "/nix/store/") and $held)
    "nix fmt keeps the formatter under a GC root" $"link '($link)' -> '($target)', held: ($held)"))
  # `iets cache gc` walks the roots store: a zero TTL drops the one link
  let gc = (^$iets cache gc --ttl 0 --cache-dir $fmtcache | complete)
  $ok = ($ok | append (verdict ($gc.exit_code == 0 and ($gc.stdout | str contains $"($fmtcache)/roots: 1 of 1 entries") and (not ($link | path exists)))
    "iets cache gc sweeps the roots store" $"exit ($gc.exit_code), stdout ($gc.stdout)"))
  hide-env IETS_CACHE_DIR

  # a flake with no `formatter` output: nix's own message, and no fallback
  cd $fdir
  let nofmt = (run-both $farm nix fmt)
  let want_msg = $"does not provide attribute 'formatter.($sys)'"
  $ok = ($ok | append (cmp-case "nix fmt without a formatter output" $nofmt))
  $ok = ($ok | append (verdict (($nofmt.ours.stderr | str contains $want_msg) and ($nofmt.real.stderr | str contains $want_msg) and (not ($nofmt.ours.stderr | str contains "falling back")))
    "nix fmt names the missing attribute itself" $"ours stderr '($nofmt.ours.stderr)'"))
  cd $tmp

  $ok = ($ok | append (cache-cases $farm $tmp $np $sys))

  # --- fallbacks, priced with IETS_NO_FALLBACK so the real run never happens
  for case in [
    {label: "nix fmt --json falls back", want: "'--json' is not implemented"
     args: [nix fmt --json]}
    {label: "nix-shell -p falls back", want: "'-p' is not implemented"
     args: [nix-shell -p hello --run "true"]}
    {label: "nix develop --phase falls back", want: "'--phase' is not implemented"
     args: [nix develop --phase build]}
    {label: "nix develop --profile falls back", want: "'--profile' is not implemented"
     args: [nix develop --profile p]}
    {label: "nix develop --redirect falls back", want: "'--redirect' is not implemented"
     args: [nix develop --redirect a b]}
  ] {
    let err = (fallback-reason $farm $case.args)
    $ok = ($ok | append (verdict ($err | str contains $case.want) $case.label $"stderr was '($err)'"))
  }
  $ok = ($ok | append (cmp-case "nix-shell <nixpkgs> fetches the sha256-pinned tarball of nixpkgs/shell.nix itself"
    (with-env {IETS_NO_FALLBACK: "1"} { run-both $farm nix-shell -I $"nixpkgs=($np)" "<nixpkgs>" -A hello --run 'echo $name' })))
  let shebang = $"($tmp)/script.sh"
  "#! /usr/bin/env nix-shell\n#! nix-shell -p hello -i bash\necho hi\n" | save -f $shebang
  let err = (fallback-reason $farm [nix-shell $shebang])
  $ok = ($ok | append (verdict ($err | str contains "a '#!' script") "nix-shell falls back on a #! script" $"stderr was '($err)'"))

  let meta = (^$"($farm)/nix" flake metadata $fref | complete)
  $ok = ($ok | append (verdict ($meta.stderr | str contains "falling back to nix: subcommand 'flake'")
    "nix flake metadata falls back" $"stderr was '($meta.stderr)'"))
  for case in [
    {ref: "nixpkgs#lib.version", want: "flake registry references (nixpkgs#…) are not supported"}
    {ref: "github:NixOS/nixpkgs#lib.version", want: "only local paths are supported"}
  ] {
    let err = (fallback-reason $farm [nix eval $case.ref])
    $ok = ($ok | append (verdict (($err | str contains "falling back to nix: iets: ") and ($err | str contains $case.want))
      $"nix eval ($case.ref) falls back" $"stderr was '($err)'"))
    let r = (^$iets eval $case.ref | complete)
    $ok = ($ok | append (verdict ($r.exit_code == 1 and ($r.stderr | str contains $case.want))
      $"iets eval ($case.ref) says why" $"exit ($r.exit_code), stderr '($r.stderr)'"))
  }
  let apply = (^$"($farm)/nix" eval --apply "x: x" --expr 1 | complete)
  $ok = ($ok | append (verdict ($apply.stderr | str contains "'--apply' is not implemented")
    "nix eval --apply falls back" $"stderr was '($apply.stderr)'"))

  cd /
  # The scratch store's directories are read-only, as every Nix store is.
  ^chmod -R u+w $"($tmp)/scratch" err> /dev/null | complete | ignore
  rm -rf $tmp
  let failed = ($ok | where $it == false | length)
  print $"($ok | length) cases, ($failed) failed"
  if $failed > 0 { exit 1 }
}
