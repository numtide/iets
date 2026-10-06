#!/usr/bin/env nu
# End-to-end test of `iets build` against a Nix daemon (`--builder daemon`,
# `unix://`).
#
# 1. custom derivation with a source tree through the local daemon, drv and
#    source deleted first; checks that the registered paths are byte-identical
#    to what nix-instantiate created (nix-store -q --hash, nix derivation show,
#    nix path-info ca), and --fetch-to.
# 2. hello through the local daemon: the closure is GC-rooted here, so every
#    path is valid and only QueryValidPaths and the realise run.
# 3. hello cold against a scratch chroot daemon (nix-daemon --store
#    local?root=DIR on its own socket), --no-build: the whole closure uploaded in one
#    AddMultipleToStore; hashes and ca checked in that store.
# 4. hello warm against the scratch daemon: 0 missing.
# 5. custom derivation through unix:///nix/var/nix/daemon-socket/socket.
# 6. custom derivation through ssh-ng://, with an `ssh` shim in PATH that runs
#    `nix-daemon --stdio` locally: the stdio transport without a remote host.
# 7. an input-addressed build output (`builtins.storePath` of the hello
#    output) as an input source, through the local daemon that has it: no
#    upload of the source, no `substitute:` line.
# 8. the same drv through a second scratch daemon that lacks the output and
#    substitutes it from a file:// binary cache made with `nix copy`: first
#    --no-build (the daemon substitutes, then the drv registers), then the
#    build itself.
# 9. the stdout contract (docs/reference/commands.md): a run that exits 0 put a
#    store path on stdout, and every run that puts none exits non-zero with an
#    error on stderr.
#
# usage: scripts/daemon-e2e.nu            # uses target/release
#        BIN=target/debug scripts/daemon-e2e.nu
use lib.nu [ROOT bytes-of chomp current-system fail iets-bin isolate nixpkgs out-of quiet-ok require-real-nix run-in same-file scratch-daemon show show-matching]

def path-info [fields: string, p: string, ...store: string]: nothing -> string {
  let r = (^nix path-info ...$store --json $p | complete)
  if $r.exit_code != 0 { error make -u {msg: $"nix path-info ($p) failed"} }
  $r.stdout | ^jq -c $".[] | ($fields)"
}

# make-case DIR NAME: writes DIR/src and DIR/case.nix, a derivation that copies the tree.
def make-case [dir: string, name: string, system: string] {
  mkdir $"($dir)/src/sub"
  $"hello (random int 0..32767) ($nu.pid) (date now | into int)\n" | save $"($dir)/src/a"
  "#!/bin/sh\necho hi\n" | save $"($dir)/src/run.sh"
  ^chmod +x $"($dir)/src/run.sh"
  "zzz\n" | save $"($dir)/src/sub/b"
  ^ln -s a $"($dir)/src/link"
  r#'derivation {
  name = "@NAME@";
  system = "@SYSTEM@";
  builder = "/bin/sh";
  args = [ "-c" "while read -r l; do echo \"$l\"; done < ${@DIR@/src}/a > $out; while read -r l; do echo \"$l\"; done < ${@DIR@/src}/sub/b >> $out" ];
}
'# | str replace -a "@NAME@" $name | str replace -a "@SYSTEM@" $system | str replace -a "@DIR@" $dir | save $"($dir)/case.nix"
}

# The lines `--timings` prints, in the client flow's order.
const timing_lines = '^(eval|plan|connect|query|resolve|substitute|add|build|outputs|fetch|worker):'

# make-storepath-case DIR NAME SYSTEM OUT: writes DIR/case.nix, a derivation that runs the
# hello output OUT, pinned with builtins.storePath, into $out.
def make-storepath-case [dir: string, name: string, system: string, out: string] {
  mkdir $dir
  r#'derivation {
  name = "@NAME@";
  system = "@SYSTEM@";
  builder = "/bin/sh";
  args = [ "-c" "${builtins.storePath "@OUT@"}/bin/hello > $out" ];
}
'# | str replace -a "@NAME@" $name | str replace -a "@SYSTEM@" $system | str replace -a "@OUT@" $out | save $"($dir)/case.nix"
}

# run-custom DIR URL: instantiate, record what Nix created, delete, build via URL, compare.
def run-custom [iets: string, dir: string, url: string] {
  let drv = (^nix-instantiate $"($dir)/case.nix" err> /dev/null | chomp)
  let src = (out-of nix-store -q --references $drv | chomp)
  let expected = (out-of nix-store -q --outputs $drv | chomp)
  print $"drv: ($drv)"
  print $"src: ($src)"
  cp $drv $"($dir)/nix.drv"
  out-of nix-store -q --hash $drv $src | save -f $"($dir)/nix.hashes"
  ^nix derivation show $drv err> /dev/null | save -f $"($dir)/nix.show"
  path-info "{ca, narHash, narSize, references}" $src | save -f $"($dir)/nix.src-info"
  path-info "{ca, narHash, narSize, references}" $drv | save -f $"($dir)/nix.drv-info"
  if not (quiet-ok nix-store --delete $drv $src) { fail $"cannot delete ($drv) and ($src) from the store" }
  if (quiet-ok nix-store --check-validity $src) { fail "source still valid after delete" }
  let r = (^$iets build --builder $url --timings --no-tracing-cache --fetch-to $"($dir)/fetch" $"($dir)/case.nix" | complete)
  $r.stderr | save -f $"($dir)/client.log"
  if $r.exit_code != 0 { print ($r.stderr | chomp); fail "client exited non-zero" }
  let out = ($r.stdout | chomp)
  show-matching $timing_lines $"($dir)/client.log"
  if ($r.stderr | str contains "read from /nix/store") { fail "eval path fell back to the local store" }
  if $out != $expected { fail $"output '($out)' != expected '($expected)'" }
  if ((bytes-of $"($dir)/src/a") ++ (bytes-of $"($dir)/src/sub/b")) != (bytes-of $out) { fail "output content differs" }
  if not (quiet-ok nix-store --check-validity $drv $src) { fail "drv or source not valid after build" }
  if not (same-file $drv $"($dir)/nix.drv") { fail "registered drv differs from nix-instantiate's" }
  if (out-of nix-store -q --hash $drv $src | into binary) != (bytes-of $"($dir)/nix.hashes") { fail "nix-store -q --hash differs" }
  if (^nix derivation show $drv err> /dev/null | into binary) != (bytes-of $"($dir)/nix.show") { fail "nix derivation show differs" }
  if (path-info "{ca, narHash, narSize, references}" $src | into binary) != (bytes-of $"($dir)/nix.src-info") { fail "source path-info differs" }
  if (path-info "{ca, narHash, narSize, references}" $drv | into binary) != (bytes-of $"($dir)/nix.drv-info") { fail "drv path-info differs" }
  if not (quiet-ok test -x $"($src)/run.sh") { fail "restored tree lost the executable bit" }
  if (^readlink $"($src)/link" | chomp) != "a" { fail "restored tree lost the symlink" }
  if not (same-file $"($dir)/fetch/($out | path basename)" $out) { fail "--fetch-to copy differs from the output" }
  print $"ok: ($out)"
}

def run-tests [iets: string, nixpkgs: string, work: string] {
  let system = (current-system)

  print "== 1. custom derivation with a source tree, --builder daemon"
  make-case $"($work)/c1" iets-daemon-e2e $system
  run-custom $iets $"($work)/c1" daemon

  print "== 2. hello through the local daemon (closure already valid)"
  let hello = (^nix-instantiate -I $"nixpkgs=($nixpkgs)" '<nixpkgs>' -A hello err> /dev/null | chomp)
  let expected = (out-of nix-store -q --outputs $hello | chomp)
  let r = (^$iets build --builder daemon --impure --timings --no-tracing-cache -I $"nixpkgs=($nixpkgs)" -E '(import <nixpkgs> {}).hello' | complete)
  $r.stderr | save -f $"($work)/client2.log"
  if $r.exit_code != 0 { print ($r.stderr | chomp); fail "client exited non-zero" }
  let out = ($r.stdout | chomp)
  show-matching $timing_lines $"($work)/client2.log"
  if ($r.stderr | str contains "read from /nix/store") { fail "eval path fell back to the local store" }
  if $out != $expected { fail $"hello output '($out)' != expected '($expected)'" }
  print $"ok: ($out)"

  print "== 3. hello cold against a scratch chroot daemon, --no-build"
  let scratch = $"local?root=($work)/store"
  let sock = (scratch-daemon $"($work)/store")
  let r = (^$iets build --builder $"unix://($sock)" --timings --no-tracing-cache --no-build --impure -I $"nixpkgs=($nixpkgs)" -E '(import <nixpkgs> {}).hello' | complete)
  $r.stderr | save -f $"($work)/client3.log"
  if $r.exit_code != 0 { print ($r.stderr | chomp); fail "client exited non-zero" }
  show-matching $timing_lines $"($work)/client3.log"
  # the size of hello's closure moves with the nixpkgs pin
  let q = ($r.stderr | parse -r 'query: (?<paths>\d+) paths, (?<missing>\d+) missing' | get -o 0 | default {paths: "0", missing: ""})
  if $q.paths != $q.missing or ($q.paths | into int) < 100 { fail "cold scratch store should miss every path" }
  if not ($r.stderr | str contains "worker: 3 ops") { fail "expected SetOptions + QueryValidPaths + AddMultipleToStore" }
  if (out-of nix-store --store $scratch -q --hash $hello) != (out-of nix-store -q --hash $hello) { fail "hello drv hash differs in the scratch store" }
  if not (same-file $"($work)/store($hello)" $hello) { fail "hello drv bytes differ in the scratch store" }
  let shown = (^nix derivation show --store $scratch $hello err> /dev/null | complete)
  let name = ($shown.stdout | from json | get derivations | transpose k v | where k == ($hello | path basename) | get -o 0.v.name)
  if $shown.exit_code != 0 or $name != "hello-2.12.3" { fail "nix derivation show failed in the scratch store" }
  for p in (out-of nix-store -q --references $hello | lines | where not ($it | str ends-with ".drv")) {
    let a = (^nix path-info --store $scratch --json $p err> /dev/null | ^jq -c '.[] | {ca, narHash, references}' | chomp)
    let b = (^nix path-info --json $p err> /dev/null | ^jq -c '.[] | {ca, narHash, references}' | chomp)
    if $a != $b { fail $"path-info differs for ($p): ($a) vs ($b)" }
  }
  print $"ok: ($q.paths) paths registered, hashes and ca match the local store"

  print "== 4. hello warm against the scratch daemon"
  let r = (^$iets build --builder $"unix://($sock)" --timings --no-tracing-cache --no-build --impure -I $"nixpkgs=($nixpkgs)" -E '(import <nixpkgs> {}).hello' | complete)
  $r.stderr | save -f $"($work)/client4.log"
  if $r.exit_code != 0 { print ($r.stderr | chomp); fail "client exited non-zero" }
  show-matching $timing_lines $"($work)/client4.log"
  if not ($r.stderr | str contains $"query: ($q.paths) paths, 0 missing") { fail "warm run still had missing paths" }
  print "ok"

  print "== 5. custom derivation, --builder unix:///nix/var/nix/daemon-socket/socket"
  make-case $"($work)/c5" iets-daemon-e2e-unix $system
  run-custom $iets $"($work)/c5" unix:///nix/var/nix/daemon-socket/socket

  print "== 6. custom derivation, --builder ssh-ng://shim (ssh shim runs nix-daemon --stdio)"
  mkdir $"($work)/bin"
  "#!/bin/sh\n# ignore the host and the remote command: run the daemon on stdio here\nexec nix-daemon --stdio\n" | save $"($work)/bin/ssh"
  ^chmod +x $"($work)/bin/ssh"
  make-case $"($work)/c6" iets-daemon-e2e-ssh $system
  with-env {PATH: ($env.PATH | prepend $"($work)/bin")} { run-custom $iets $"($work)/c6" ssh-ng://shim }

  print "== 7. builtins.storePath of the hello output as an input source, local daemon has it"
  let hello_out = $expected
  make-storepath-case $"($work)/c7" iets-daemon-e2e-storepath $system $hello_out
  let drv7 = (^nix-instantiate $"($work)/c7/case.nix" err> /dev/null | chomp)
  if not (out-of nix-store -q --references $drv7 | lines | any {|l| $l == $hello_out }) { fail $"nix-instantiate did not list ($hello_out) as an input source" }
  let expected7 = (out-of nix-store -q --outputs $drv7 | chomp)
  cp $drv7 $"($work)/c7/nix.drv"
  if not (quiet-ok nix-store --delete $drv7) { fail $"cannot delete ($drv7) from the store" }
  let r = (^$iets build --builder daemon --timings --no-tracing-cache --option pure-eval false $"($work)/c7/case.nix" | complete)
  $r.stderr | save -f $"($work)/client7.log"
  if $r.exit_code != 0 { print ($r.stderr | chomp); fail "client exited non-zero" }
  show-matching $timing_lines $"($work)/client7.log"
  if not ($r.stderr | str contains "query: 2 paths, 1 missing") { fail "the local daemon should have the hello output and lack the drv" }
  if ($r.stderr | str contains "substitute:") { fail "nothing should be left to the daemon when it has the path" }
  let out7 = ($r.stdout | chomp)
  if $out7 != $expected7 { fail $"output '($out7)' != expected '($expected7)'" }
  if (open --raw $out7 | chomp) != "Hello, world!" { fail "output content differs" }
  if not (same-file $drv7 $"($work)/c7/nix.drv") { fail "registered drv differs from nix-instantiate's" }
  print $"ok: ($out7)"

  print "== 8. the same through a scratch daemon that lacks the output and substitutes it from file://"
  let cache = $"file://($work)/cache"
  if not (quiet-ok nix copy --to $"($cache)?compression=none" $hello_out) { fail $"cannot copy ($hello_out) to ($cache)" }
  let scratch2 = $"local?root=($work)/store2"
  let sock2 = (scratch-daemon $"($work)/store2" --option substituters $cache --option require-sigs "false")
  if (quiet-ok nix-store --store $scratch2 --check-validity $hello_out) { fail "the second scratch store should start without the hello output" }
  let r = (^$iets build --builder $"unix://($sock2)" --timings --no-tracing-cache --no-build --option pure-eval false $"($work)/c7/case.nix" | complete)
  $r.stderr | save -f $"($work)/client8a.log"
  if $r.exit_code != 0 { print ($r.stderr | chomp); fail "client exited non-zero" }
  show-matching $timing_lines $"($work)/client8a.log"
  if not ($r.stderr | str contains "query: 2 paths, 2 missing") { fail "the scratch daemon should lack both paths" }
  if not ($r.stderr | str contains "substitute: 1 paths left to the daemon") { fail "the hello output should be left to the daemon" }
  if not ($r.stderr | str contains "add: 1 paths") { fail "only the drv should be uploaded" }
  if not (quiet-ok nix-store --store $scratch2 --check-validity $hello_out $drv7) { fail "hello output or drv not valid in the scratch store after --no-build" }
  if not (same-file $"($work)/store2($drv7)" $drv7) { fail "drv bytes differ in the scratch store" }
  print "ok: substituted, drv registered"
  let r = (^$iets build --builder $"unix://($sock2)" --timings --no-tracing-cache --option pure-eval false $"($work)/c7/case.nix" | complete)
  $r.stderr | save -f $"($work)/client8b.log"
  if $r.exit_code != 0 { print ($r.stderr | chomp); fail "client exited non-zero" }
  show-matching $timing_lines $"($work)/client8b.log"
  if not ($r.stderr | str contains "query: 2 paths, 0 missing") { fail "the second run should find both paths" }
  let out8 = ($r.stdout | chomp)
  if $out8 != $expected7 { fail $"output '($out8)' != expected '($expected7)'" }
  if (open --raw $"($work)/store2($out8)" | chomp) != "Hello, world!" { fail "output content differs in the scratch store" }
  print $"ok: ($out8)"

  print "== 9. the stdout contract: a path on stdout, or a non-zero exit"
  run-contract $iets $work $system
  print "PASS"
}

# The stdout contract of `iets build` (docs/reference/commands.md). A caller
# that captures stdout gets a store path or a failure, never silence and
# success: a deploy script fed the empty capture to `nix-env --set` and got
# `selector '' matches no derivations` instead of a diagnosis.
def run-contract [iets: string, work: string, system: string] {
  let dir = $"($work)/c9"
  make-case $dir iets-contract-ok $system
  let ok = (^$iets build --builder daemon --no-tracing-cache $"($dir)/case.nix" | complete)
  if $ok.exit_code != 0 { print ($ok.stderr | chomp); fail "the successful build exited non-zero" }
  if ($ok.stdout | chomp) == "" { fail "exit 0 with nothing on stdout" }
  print $"ok: ($ok.stdout | chomp)"

  r#'derivation {
  name = "iets-contract-fail";
  system = "@SYSTEM@";
  builder = "/bin/sh";
  args = [ "-c" "exit 3" ];
}
'# | str replace -a "@SYSTEM@" $system | save -f $"($dir)/fail.nix"
  let bad = (^$iets build --builder daemon --no-tracing-cache $"($dir)/fail.nix" | complete)
  if $bad.exit_code == 0 { fail "a failed build exited 0" }
  if ($bad.stdout | chomp) != "" { fail $"a failed build printed '($bad.stdout)' on stdout" }
  print $"ok: a failed build exits ($bad.exit_code) with no path"

  let empty = (^$iets build --builder daemon --no-tracing-cache -E '[]' | complete)
  if $empty.exit_code == 0 { fail "an expression with no derivation exited 0" }
  if ($empty.stdout | chomp) != "" { fail "an expression with no derivation printed on stdout" }
  if not ($empty.stderr | str contains "nothing to build") { fail $"no error names it: ($empty.stderr)" }
  let none = (^$iets build --builder daemon --no-tracing-cache --no-build -E '[]' | complete)
  if $none.exit_code != 1 { fail $"--no-build over nothing exited ($none.exit_code)" }
  let wrong = (^$iets build --builder daemon --no-tracing-cache --out bogus $"($dir)/case.nix" | complete)
  if $wrong.exit_code == 0 { fail "--out with an output the derivation lacks exited 0" }
  if ($wrong.stdout | chomp) != "" { fail "--out with an unknown output printed on stdout" }
  print "ok: no silent success"
}

def main [] {
  require-real-nix
  let iets = (iets-bin)
  cd $ROOT
  let work = (isolate daemon-e2e)
  run-in $work { run-tests $iets (nixpkgs) $work }
}
