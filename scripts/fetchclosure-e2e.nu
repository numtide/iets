#!/usr/bin/env nu
# builtins.fetchClosure against the real nix. Each side evaluates through its
# own scratch daemon (nix-daemon --store local?root=DIR), which trusts one
# test key, and fetches from file:// binary caches made with `nix copy`
# (`_NIX_IN_TEST` lets both take file://). Compared: the value and its
# context, the error message, and the path info each store ends up with.
#
# 1. a content-addressed closure: two toFile paths, one naming the other.
# 2. an input-addressed closure (hello) from a cache the daemon does not
#    trust: the daemon refuses it.
# 3. the same from a cache signed with the trusted key: copied.
# 4. hello without `inputAddressed`: refused, it is input-addressed.
# 5. hello rewritten to content-addressed paths: `toPath = ""` names the
#    path, and that `toPath` gives it.
# 6. argument errors.
#
# usage: nix develop -c nu scripts/fetchclosure-e2e.nu
#        BIN=target/debug nix develop -c nu scripts/fetchclosure-e2e.nu
use lib.nu [chomp fail iets-bin isolate make-farm nixpkgs out-of require-real-nix run-in scratch-daemon verdict]

require-real-nix nix

# The message of the first level-0 `--log-format internal-json` error record.
def err-msg [stderr: string]: nothing -> string {
  $stderr | lines | where ($it | str starts-with "@nix ") | each { str substring 5.. | from json }
  | where {|r| $r.level? == 0 and ($r.raw_msg? != null) } | get -o 0.raw_msg | default "" | ansi strip
}

# EXPR through the real nix on store A and through iets on store B.
def eval-both [farm: string, a: string, b: string, expr: string]: nothing -> record {
  let args = [--extra-experimental-features "nix-command fetch-closure" eval --json --log-format internal-json --expr $expr]
  with-env {_NIX_IN_TEST: "1"} {
    let real = (^nix --store $"unix://($a)" ...$args | complete)
    let ours = (^$"($farm)/nix" --store $"unix://($b)" ...$args | complete)
    if ($ours.stderr | str contains "falling back to nix") {
      fail $"iets fell back to nix: ($ours.stderr | str substring 0..400)"
    }
    {real: $real, ours: $ours}
  }
}

def same-value [label: string, r: record]: nothing -> bool {
  let ok = $r.real.exit_code == 0 and $r.ours.exit_code == 0 and $r.real.stdout == $r.ours.stdout
  verdict $ok $label $"($label): real ($r.real.exit_code) ($r.real.stdout | str trim) ($r.real.stderr | str substring 0..300)\n  ours ($r.ours.exit_code) ($r.ours.stdout | str trim) ($r.ours.stderr | str substring 0..300)"
}

def same-error [label: string, r: record]: nothing -> bool {
  let a = (err-msg $r.real.stderr)
  let b = (err-msg $r.ours.stderr)
  let ok = $r.real.exit_code != 0 and $r.ours.exit_code != 0 and $a != "" and $a == $b
  verdict $ok $label $"($label): exit real ($r.real.exit_code) ours ($r.ours.exit_code)\n  real: ($a)\n  ours: ($b)"
}

# The path info of P's closure in the store under ROOT, without the times.
def infos [root: string, p: string]: nothing -> string {
  out-of nix --extra-experimental-features nix-command path-info --store $"local?root=($root)" --json --json-format 1 --recursive $p
  | from json | transpose path info
  | each {|e| {path: $e.path, ca: $e.info.ca, narHash: $e.info.narHash, narSize: $e.info.narSize, references: $e.info.references}}
  | sort-by path | to json -r
}

let iets = (iets-bin)
let work = (isolate fetchclosure-e2e)
run-in $work {
  let np = (nixpkgs)
  let farm = (make-farm $iets $"($work)/farm")
  let key = $"($work)/key.sec"
  out-of nix --extra-experimental-features nix-command key generate-secret --key-name fetchclosure-e2e-1 | save -f $key
  let pub = (open --raw $key | ^nix --extra-experimental-features nix-command key convert-secret-to-public | chomp)
  let a = (scratch-daemon $"($work)/a" --option trusted-public-keys $pub)
  let b = (scratch-daemon $"($work)/b" --option trusted-public-keys $pub)
  mut ok = []

  print "== 1. a content-addressed closure"
  let nonce = $"(random int 0..999999)-(date now | into int)"
  let top = (out-of nix --extra-experimental-features nix-command eval --raw --expr $'builtins.toFile "fc-top-($nonce)" "${builtins.toFile "fc-dep-($nonce)" "($nonce)"}"')
  let ca = $"file://($work)/ca"
  out-of nix --extra-experimental-features nix-command copy --to $ca $top | ignore
  let r = (eval-both $farm $a $b $"let r = builtins.fetchClosure { fromStore = \"($ca)\"; fromPath = ($top); }; in [r \(builtins.getContext r\) \(builtins.readFile r\)]")
  $ok = ($ok | append (same-value "content-addressed closure" $r))
  $ok = ($ok | append (verdict ((infos $"($work)/a" $top) == (infos $"($work)/b" $top)) "the same path infos" "the path infos differ"))

  print "== 2. an input-addressed closure from an untrusted cache"
  let hello = (out-of nix-build --no-out-link -I $"nixpkgs=($np)" -E "(import <nixpkgs> {}).hello" | chomp)
  let unsigned = $"file://($work)/unsigned"
  out-of nix --extra-experimental-features nix-command copy --to $"($unsigned)?compression=none" $hello | ignore
  let ia = $"builtins.fetchClosure { fromStore = \"($unsigned)\"; fromPath = ($hello); inputAddressed = true; }"
  $ok = ($ok | append (same-error "unsigned input-addressed closure" (eval-both $farm $a $b $ia)))

  print "== 3. the same, signed by a trusted key"
  let signed = $"file://($work)/signed"
  out-of nix --extra-experimental-features nix-command copy --to $"($signed)?compression=zstd&secret-key=($key)" $hello | ignore
  let ia = $"let r = builtins.fetchClosure { fromStore = \"($signed)\"; fromPath = ($hello); inputAddressed = true; }; in [r \(builtins.getContext r\)]"
  $ok = ($ok | append (same-value "signed input-addressed closure" (eval-both $farm $a $b $ia)))
  $ok = ($ok | append (verdict ((infos $"($work)/a" $hello) == (infos $"($work)/b" $hello)) "the same path infos" "the path infos differ"))

  print "== 4. an input-addressed path, asked for as content-addressed"
  let wrong = $"builtins.fetchClosure { fromStore = \"($signed)\"; fromPath = ($hello); }"
  $ok = ($ok | append (same-error "input-addressed without inputAddressed" (eval-both $farm $a $b $wrong)))

  print "== 5. rewritten to content-addressed paths"
  let ask = (eval-both $farm $a $b $"builtins.fetchClosure { fromStore = \"($unsigned)\"; fromPath = ($hello); toPath = \"\"; }")
  $ok = ($ok | append (same-error "toPath = \"\" names the path" $ask))
  let to = (err-msg $ask.real.stderr | parse -r "yielded '(?<p>[^']+)'" | get -o 0.p | default "")
  if $to == "" { fail "no rewritten path in the error" }
  let rw = $"let r = builtins.fetchClosure { fromStore = \"($unsigned)\"; fromPath = ($hello); toPath = ($to); }; in [r \(builtins.getContext r\)]"
  $ok = ($ok | append (same-value "toPath gives the rewritten path" (eval-both $farm $a $b $rw)))
  $ok = ($ok | append (verdict ((infos $"($work)/a" $to) == (infos $"($work)/b" $to)) "the same path infos" "the path infos differ"))
  let other = $"builtins.fetchClosure { fromStore = \"($unsigned)\"; fromPath = ($hello); toPath = /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello; }"
  $ok = ($ok | append (same-error "a toPath that is not the rewrite" (eval-both $farm $a $b $other)))
  # a valid toPath is not rewritten again, only checked
  let valid = $"builtins.fetchClosure { fromStore = \"($unsigned)\"; fromPath = ($hello); toPath = ($top); }"
  $ok = ($ok | append (same-value "a valid content-addressed toPath" (eval-both $farm $a $b $valid)))

  print "== 6. argument errors"
  for e in [
    $"builtins.fetchClosure { fromStore = \"($ca)\"; }"
    $"builtins.fetchClosure { fromPath = ($top); }"
    $"builtins.fetchClosure { fromPath = ($top); fromStore = \"($ca)\"; bogus = 1; }"
    $"builtins.fetchClosure { fromPath = ($top); fromStore = \"s3://bucket\"; }"
    $"builtins.fetchClosure { fromPath = ($top); fromStore = \"https://example.org?priority=1\"; }"
    $"builtins.fetchClosure { fromPath = \"($top)/x\"; fromStore = \"($ca)\"; }"
    $"builtins.fetchClosure { fromPath = ($hello); fromStore = \"($ca)\"; toPath = \"\"; inputAddressed = true; }"
    $"builtins.fetchClosure { fromPath = /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-gone; fromStore = \"($ca)\"; }"
  ] {
    $ok = ($ok | append (same-error ($e | str replace -a $work WORK) (eval-both $farm $a $b $e)))
  }

  let failed = ($ok | where $it == false | length)
  print $"($ok | length) cases, ($failed) failed"
  if $failed > 0 { fail "some cases differ" }
}
