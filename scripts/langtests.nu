#!/usr/bin/env nu
# Runs tests/lang/eval-okay-*.nix and eval-fail-*.nix against iets, from
# the tests/ directory like Nix's own harness (paths print as /pwd/lang/...).
# `--impure` because the corpus is Nix's, run there by `nix-instantiate
# --eval`: cases read `/proc/version`, call `storePath` and `getFlake` on an
# unlocked reference, and use the search path. A case's own .flags come after
# it, so one that wants `--pure-eval` still gets it.
# Exits nonzero on a failure that tests/lang/expected-failures.txt does not
# list, and on a listed case that starts passing.
use lib.nu [chomp exit-code flags-of glob-sorted iets-bin]

def expected-failures []: nothing -> list<string> {
  let f = "lang/expected-failures.txt"
  if not ($f | path exists) { return [] }
  open --raw $f | lines | each {|l| $l | str trim } | where {|l| $l != "" and not ($l | str starts-with "#") }
}

def main [] {
  cd ($env.FILE_PWD | path join "..")
  let iets = (iets-bin)
  $env.HOME = "/fake-home"
  $env.TEST_VAR = "foo" # for eval-okay-getenv.nix
  cd tests
  $env.NIX_PATH = "lang/dir3:lang/dir4"
  let pwd = $env.PWD
  mut pass = 0
  mut fail = 0
  mut failed = []
  mut detail = []
  for f in (glob-sorted lang/eval-okay-*.nix) {
    let n = ($f | str replace -r '\.nix$' '')
    if not ($"($n).exp" | path exists) { continue }
    let r = (^$iets eval --impure --strict ...(flags-of $n) $f | complete)
    let out = ($r.stdout | str replace -a $pwd "/pwd" | chomp)
    if $out == (open --raw $"($n).exp" | chomp) {
      $pass += 1
    } else {
      $fail += 1
      $failed = ($failed | append ($n | path basename))
      $detail = ($detail | append ($n | path basename))
    }
  }
  for f in (glob-sorted lang/eval-fail-*.nix) {
    let n = ($f | str replace -r '\.nix$' '')
    let rc = (exit-code $iets eval --impure --strict ...(flags-of $n) $f)
    let base = ($n | path basename)
    if $rc == 0 {
      $fail += 1
      $failed = ($failed | append $base)
      $detail = ($detail | append $"($base) \(should fail\)")
    } else if $rc >= 128 {
      $fail += 1
      $failed = ($failed | append $base)
      $detail = ($detail | append $"($base) \(crashed rc=($rc)\)")
    } else {
      $pass += 1
    }
  }
  print $"pass=($pass) fail=($fail)"
  print ($detail | first ($env.SHOW? | default 40 | into int) | str join "\n")
  let expected = (expected-failures)
  let new = ($failed | where {|n| $n not-in $expected })
  let fixed = ($expected | where {|n| $n not-in $failed })
  if ($new | is-not-empty) { print $"unexpected failures: ($new | str join ', ')" }
  if ($fixed | is-not-empty) {
    print $"now passing, drop from lang/expected-failures.txt: ($fixed | str join ', ')"
  }
  if ($new | is-not-empty) or ($fixed | is-not-empty) { exit 1 }
}
