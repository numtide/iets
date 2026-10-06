#!/usr/bin/env nu
# End-to-end test of `iets build` over an attribute tree, against the local
# nix-daemon.
#
# The flake has `checks.<system>` for two systems with four tiny /bin/sh
# derivations: `ok`, `dep` (which `chain` reads) and `bad` (which exits 1);
# plus a `packages` set. Nothing needs nixpkgs.
#
# 1. DIR#checks: the current system's checks are built, the failure does not
#    stop the others, the exit code is Nix's 100, stdout has the paths built.
# 2. --json: the `plumbing build` shape, one outcome per check.
# 3. the same build again: nothing is built twice.
# 4. one root: the failing check alone, and a set of one package.
# 5. a plain Nix file with -A and --argstr.
# 6. --dry-run over a tree.
# 7. an attribute that does not evaluate, and a tree with nothing in it.
#
# usage: nu scripts/build-e2e.nu
#        BIN=target/debug nu scripts/build-e2e.nu
use lib.nu [current-system checks-flake iets-bin isolate verdict]

def --wrapped go [...cmd: string]: nothing -> record {
  ^($cmd | first) ...($cmd | skip 1) | complete
}

def main [] {
  let iets = (iets-bin)
  let tmp = (isolate build-e2e)
  let system = (current-system)
  let flake = $"($tmp)/flake"
  checks-flake $flake
  let checks = $"path:($flake)#checks"
  let names = ([bad chain dep ok] | each {|n| $"checks.($system).($n)" })
  mut ok = []

  print "== 1. build path:DIR#checks"
  let r1 = (go $iets build --builder daemon $checks)
  let paths = ($r1.stdout | lines)
  $ok = ($ok | append (verdict ($r1.exit_code == 100) "a failing check exits 100" $"exit ($r1.exit_code)"))
  $ok = ($ok | append (verdict ($r1.stderr =~ $"checks.($system).bad failed") "the failing attribute is named" $r1.stderr))
  $ok = ($ok | append (verdict ($r1.stderr =~ 'about to fail') "the failure log tail is printed" $r1.stderr))
  $ok = ($ok | append (verdict ($r1.stderr =~ 'building fb-chain') "the checks after the failure built" $r1.stderr))
  $ok = ($ok | append (verdict ($r1.stderr =~ $"3 built, 1 failed: checks.($system).bad") "the closing line counts them" $r1.stderr))
  $ok = ($ok | append (verdict (($paths | each {|p| $p | str replace -r '^/nix/store/[^-]+-' '' }) == [fb-chain fb-dep fb-ok]) "stdout has one path per output built" $r1.stdout))
  $ok = ($ok | append (verdict ($paths | all {|p| $p | path exists }) "every path is there" $r1.stdout))
  $ok = ($ok | append (verdict (not ($r1.stderr =~ 'aarch64-linux')) "the other system is not evaluated into the build" $r1.stderr))

  print "== 2. --json"
  let r2 = (go $iets build --builder daemon --json $checks)
  let j = ($r2.stdout | from json)
  let bad = ($j.outcomes | where attr == $"checks.($system).bad" | first)
  let goods = ($j.outcomes | where attr != $"checks.($system).bad")
  $ok = ($ok | append (verdict ($r2.exit_code == 100) "--json keeps the exit code" $"exit ($r2.exit_code)"))
  $ok = ($ok | append (verdict (($j.outcomes | get attr) == $names) "one outcome per check, by attribute" $"($j.outcomes | get attr)"))
  $ok = ($ok | append (verdict (($bad | columns | sort) == [attr drv logTail outputs reason success]) "the outcome has the plumbing keys" $"($bad | columns)"))
  $ok = ($ok | append (verdict ((not $bad.success) and ($bad.outputs == {}) and ($bad.reason =~ 'fb-bad.drv')) "the failure has a reason and no output" $"($bad)"))
  $ok = ($ok | append (verdict ($bad.logTail | any {|l| $l =~ 'about to fail' }) "the failure has its log tail" $"($bad.logTail)"))
  $ok = ($ok | append (verdict ($goods | all {|o| $o.success and ($o.outputs.out in $paths) }) "the others have the paths of case 1" $"($goods)"))

  print "== 3. what is built is not built again"
  $ok = ($ok | append (verdict (not ($r2.stderr =~ 'building fb-(ok|dep|chain)')) "no second build of a valid output" $r2.stderr))
  $ok = ($ok | append (verdict ($r2.stderr =~ 'about to fail') "the failure is tried again" $r2.stderr))

  print "== 4. one root"
  let r4 = (go $iets build --builder daemon $"($checks).($system).bad")
  $ok = ($ok | append (verdict (($r4.exit_code == 100) and ($r4.stdout == "")) "the failing check alone: exit 100, no path" $"exit ($r4.exit_code) ($r4.stdout)"))
  $ok = ($ok | append (verdict (not ($r4.stderr =~ ' built, ')) "one root has no closing line" $r4.stderr))
  let r4p = (go $iets build --builder daemon $"path:($flake)#packages")
  $ok = ($ok | append (verdict (($r4p.exit_code == 0) and ($r4p.stdout =~ '-fb-lone\n$')) "#packages builds its one member" $"exit ($r4p.exit_code) ($r4p.stdout)"))
  let r4o = (go $iets build --builder daemon $"path:($flake)#checks.aarch64-linux" --dry-run --json)
  $ok = ($ok | append (verdict (($r4o.stdout | from json | get willBuild | length) == 4) "a system named in full is walked as it is" $r4o.stdout))

  print "== 5. a file, -A and --argstr"
  let rel = $"($tmp)/release.nix"
  $"{ system ? builtins.currentSystem }:
let f = import ($flake)/flake.nix; o = f.outputs { self = null; };
in { ci = o.checks.${system}; }
" | save -f $rel
  let r5 = (go $iets build --builder daemon $rel -A ci --argstr system $system -k)
  $ok = ($ok | append (verdict ($r5.exit_code == 100) "-A ci reaches the failing check" $"exit ($r5.exit_code)"))
  $ok = ($ok | append (verdict ($r5.stderr =~ 'ci.bad failed') "the failure is named by its attribute path" $r5.stderr))
  $ok = ($ok | append (verdict (($r5.stdout | lines) == $paths) "the same paths as the flake gave" $r5.stdout))

  print "== 6. --dry-run over a tree"
  checks-flake $"($tmp)/fresh"
  let r6 = (go $iets build --builder daemon --dry-run --json $"path:($tmp)/fresh#checks")
  let m = ($r6.stdout | from json)
  $ok = ($ok | append (verdict (($r6.exit_code == 0) and (($m.willBuild | length) == 4)) "the plan has the four checks" $r6.stdout))

  print "== 7. no verdict without an evaluation"
  # --impure: an `-E` expression's project is the working directory, and this
  # one imports the scratch flake under TMPDIR (docs/reference/settings.md)
  let r7 = (go $iets build --builder daemon --impure --no-tracing-cache -E $"let f = import ($tmp)/fresh/flake.nix; in \(f.outputs { self = null; }\).checks.($system) // { broken = throw \"boom\"; }")
  $ok = ($ok | append (verdict (($r7.exit_code == 1) and ($r7.stdout == "") and ($r7.stderr =~ 'boom')) "an attribute that throws stops the run" $"exit ($r7.exit_code) ($r7.stderr)"))
  $ok = ($ok | append (verdict (not ($r7.stderr =~ 'building fb-')) "nothing was built" $r7.stderr))
  let r7e = (go $iets build --builder daemon -E '{ a = { }; }')
  $ok = ($ok | append (verdict (($r7e.exit_code == 1) and ($r7e.stderr =~ 'nothing to build')) "a tree with no derivation is an error" $"exit ($r7e.exit_code) ($r7e.stderr)"))

  print "== 8. a failure in a dependency"
  # nix-build --store "local?root=DIR" gives 100 for a dependency that failed
  # to build and 102 when it was a fixed-output build with a hash mismatch.
  let salt = (random uuid)
  let deps = $"($tmp)/deps.nix"
  $"let
  salt = \"($salt)\";
  depFail = derivation { name = \"be-depfail\"; system = \"($system)\"; builder = \"/bin/sh\"; args = [ \"-c\" \"exit 1\" ]; inherit salt; };
  depFod = derivation { name = \"be-depfod\"; system = \"($system)\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo $salt > $out\" ]; inherit salt; outputHashMode = \"flat\"; outputHash = \"sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\"; };
in {
  overFail = derivation { name = \"be-over-fail\"; system = \"($system)\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo x > $out\" ]; dep = depFail; inherit salt; };
  overFod = derivation { name = \"be-over-fod\"; system = \"($system)\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo x > $out\" ]; dep = depFod; inherit salt; };
}
" | save -f $deps
  let r8a = (go $iets build --builder daemon $deps -A overFail)
  $ok = ($ok | append (verdict ($r8a.exit_code == 100) "a dependency that failed to build exits 100" $"exit ($r8a.exit_code)"))
  let r8b = (go $iets --iets-option fix-hashes never build --builder daemon $deps -A overFod)
  $ok = ($ok | append (verdict ($r8b.exit_code == 102) "a dependency with a hash mismatch exits 102" $"exit ($r8b.exit_code)"))
  $ok = ($ok | append (verdict ($r8b.stderr =~ 'be-depfod failed') "the dependency that failed keeps no check mark" $r8b.stderr))

  print "== 9. a build log cannot forge a hash mismatch"
  # the derivation is not fixed-output, so the text is only text
  let forge = $"($tmp)/forge.nix"
  let good = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="
  $"let good = \"($good)\"; in {
  forge = derivation {
    name = \"be-forge\"; system = \"($system)\"; builder = \"/bin/sh\";
    salt = \"($salt)\";
    args = [ \"-c\" ''
      printf '%s\\n' \"@nix {\\\"action\\\":\\\"msg\\\",\\\"level\\\":0,\\\"msg\\\":\\\"hash mismatch in fixed-output derivation 'x':\\n  specified: ${good}\\n     got:    sha256-HNpsZojn8xegmQzw+pTz75/kDloh2nUWpW342gddnw0=\\\"}\" >&2
      exit 1
    '' ];
  };
}
" | save -f $forge
  let r9 = (go $iets --iets-option fix-hashes always build --builder daemon $forge -A forge)
  $ok = ($ok | append (verdict ((open --raw $forge) | str contains $good) "a forged mismatch never rewrites a hash in the file" (open --raw $forge)))
  $ok = ($ok | append (verdict (not ($r9.stderr =~ 'fixed: ')) "nothing was fixed" $r9.stderr))

  ^rm -rf $tmp
  let failed = ($ok | where $it == false | length)
  print $"($ok | length) cases, ($failed) failed"
  if $failed > 0 { exit 1 }
  print "PASS"
}
