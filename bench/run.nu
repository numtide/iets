#!/usr/bin/env nu
# Usage: bench/run.nu [workload...]   (default: nixos-desktop)
# Compares nix-instantiate, Lix, fix, and iets at 1 and 4 workers with hyperfine.
# COLD=1 drops the page cache before each run (needs root); results get a
# -cold suffix. LIX is the path to Lix's nix-instantiate; Lix is skipped when
# it is unset, and fix when its binary is absent. Every eval cache is off:
# iets --no-cache, fix --no-compile-cache; Nix and Lix have none.
# Each run writes the tool versions and the machine to bench/results/env.txt;
# set IETS_COMMIT and FIX_COMMIT to record the commits the binaries came from.
use ../scripts/lib.nu [require-real-nix]

def ver [cmd: string]: nothing -> string {
  try { ^$cmd --version | lines | first } catch { "not found" }
}

def write-pins [np: string, iets: string, fix: string] {
  let rev = ([$np .git-revision] | path join)
  let v = ([$np .version] | path join)
  [
    $"date: (date now | date to-timezone UTC | format date '%Y-%m-%dT%H:%M:%SZ')"
    $"host: (sys host | get hostname), (sys cpu | length) cores, (sys cpu | first | get brand), kernel (sys host | get kernel_version)"
    $"nix: (ver nix-instantiate)"
    $"lix: (if ($env.LIX? | is-empty) { 'not run' } else { ver $env.LIX })"
    $"fix: (if ($fix | path exists) { $'(ver $fix), ($env.FIX_COMMIT? | default "commit unknown")' } else { 'not run' })"
    $"iets: ($env.IETS_COMMIT? | default 'commit unknown'), sha256 (open --raw $iets | hash sha256 | str substring 0..15)"
    $"nixpkgs: (if ($v | path exists) { open --raw $v | str trim } else { '' }) (if ($rev | path exists) { open --raw $rev | str trim } else { $np })"
    $"hyperfine: (ver hyperfine)"
    $"load: (open --raw /proc/loadavg | str trim)"
  ] | str join "\n" | save -f bench/results/env.txt
}

def main [...workloads: string] {
  require-real-nix nix-instantiate
  cd ($env.FILE_PWD | path join "..")
  let np = ($env.NIXPKGS? | default "/nix/store/ah3pxz4xa52hd6vhbx17vcsci0b8bmbr-source")
  let fix = ($env.FIX? | default $"($env.HOME)/tmp/fix-result/bin/fix")
  let iets = ($env.IETS? | default "./target/release/iets")
  let cold = ($env.COLD? | default "" | is-not-empty)
  mkdir bench/results
  let ws = if ($workloads | is-empty) { [nixos-desktop] } else { $workloads }
  if not ($fix | path exists) { print -e $"fix: ($fix) not found, skipped" }
  write-pins $np $iets $fix
  for w in $ws {
    let f = $"($env.PWD)/bench/workloads/($w).nix"
    let out = $"bench/results/($w)(if $cold { '-cold' } else { '' })"
    let prepare = if $cold { [--prepare "sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'"] } else { [] }
    let lixcmd = if ($env.LIX? | is-empty) { [] } else { [-n lix $"($env.LIX) --eval --strict --option pure-eval false -I nixpkgs=($np) ($f)"] }
    let fixcmd = if ($fix | path exists) { [1 4] | each {|n| [-n $"fix w($n)" $"($fix) eval --strict --no-progress --no-compile-cache --workers ($n) --option pure-eval false -I nixpkgs=($np) ($f)"] } | flatten } else { [] }
    (^hyperfine --shell=none --warmup 1 --runs ($env.RUNS? | default "5") ...$prepare
      --export-markdown $"($out).md" --export-json $"($out).json"
      -n nix $"nix-instantiate --eval --strict --option pure-eval false -I nixpkgs=($np) ($f)"
      ...$lixcmd
      ...$fixcmd
      -n "iets w1" $"($iets) eval --impure --strict --no-cache --workers 1 -I nixpkgs=($np) ($f)"
      -n "iets w4" $"($iets) eval --impure --strict --no-cache --workers 4 -I nixpkgs=($np) ($f)")
  }
}
