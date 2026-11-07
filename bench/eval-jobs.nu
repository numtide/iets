#!/usr/bin/env nu
# Usage: bench/eval-jobs.nu [rounds] [--no-parity]
# `iets eval-jobs` against `nix-eval-jobs` on nixos/release-small.nix with 4
# workers, cold and warm runs interleaved. Reports the median of each cell.
# GNU time comes from the system, not the dev shell: set TIME_BIN to move it.
# Then one --force-recurse run of each tool, whose job sets must match
# (--no-parity skips it; that pair takes a few minutes).
use ../scripts/lib.nu [require-real-nix]

def timed [cache: path, env_vars: record, cmd: list<string>] {
  let out = (mktemp -t iets-bench-XXXXXX)
  let t = ($env.TIME_BIN? | default "/run/current-system/sw/bin/time")
  # IETS_WAIT_EXIT: one process, so time sees the evaluation's CPU and memory
  let r = (with-env ($env_vars | merge {IETS_WAIT_EXIT: "1"}) { ^$t -f "%e %U %S %M" -o $out ...$cmd | complete })
  let f = (open $out | str trim | split row " ")
  rm -f $out
  if $r.exit_code != 0 { print $"warning: ($cmd | first) exited ($r.exit_code)" }
  {
    wall: ($f.0 | into float)
    user: ($f.1 | into float)
    sys: ($f.2 | into float)
    rss: (($f.3 | into float) / 1024)
  }
}

def median [xs: list<float>] {
  let s = ($xs | sort)
  $s | get (($s | length) // 2)
}

def norm []: string -> list<string> {
  let out = ($in | ^jq -S -c 'if .error then {attr, error: true} else {attr, drvPath, outputs, name, system, requiredSystemFeatures} end')
  $out | lines | sort
}

# One run of each tool with --force-recurse: the job sets must be equal.
# Error text is not compared (iets renders its own traces).
def parity [iets: string, nej: string, args: list<string>]: nothing -> bool {
  let i = (^$iets eval-jobs --force-recurse ...$args | complete)
  let n = (^$nej --force-recurse ...$args | complete)
  let ii = ($i.stdout | norm)
  let nn = ($n.stdout | norm)
  if $i.exit_code == $n.exit_code and $ii == $nn {
    print $"parity ok: --force-recurse, ($ii | length) lines, exit ($i.exit_code)"
    return true
  }
  print $"parity FAIL: --force-recurse, iets ($ii | length) lines exit ($i.exit_code), nix-eval-jobs ($nn | length) lines exit ($n.exit_code)"
  $nn | where {|l| $l not-in $ii } | take 3 | each {|l| print $"  only nix-eval-jobs: ($l)" } | ignore
  $ii | where {|l| $l not-in $nn } | take 3 | each {|l| print $"  only iets: ($l)" } | ignore
  $i.stderr | lines | last 3 | each {|l| print $"  iets: ($l)" } | ignore
  false
}

def main [rounds: int = 3, --no-parity] {
  cd ($env.FILE_PWD | path join "..")
  let np = ($env.NIXPKGS? | default "/nix/store/ah3pxz4xa52hd6vhbx17vcsci0b8bmbr-source")
  let iets = ($env.IETS? | default $"($env.PWD)/target/release/iets")
  let nej = ($env.NIX_EVAL_JOBS? | default (which nix-eval-jobs | get command.0? | default $"($env.HOME)/tmp/nej/bin/nix-eval-jobs"))
  require-real-nix $nej
  let cache = $"($env.TMPDIR? | default '/tmp')/iets-bench-cache"
  let nixpkgs_arg = $"{ outPath = \"($np)\"; revCount = 1; shortRev = \"abcdefg\"; }"
  let args = ["--workers" "4" "-I" $"nixpkgs=($np)" "--arg" "nixpkgs" $nixpkgs_arg $"($np)/nixos/release-small.nix"]

  let tools = if ($nej | path exists) {
    [[name, cmd, cached]; ["iets eval-jobs", ([$iets "eval-jobs"] ++ $args), true]
                          ["nix-eval-jobs", ([$nej] ++ $args), false]]
  } else {
    print "nix-eval-jobs not found; measuring iets alone"
    [[name, cmd, cached]; ["iets eval-jobs", ([$iets "eval-jobs"] ++ $args), true]]
  }

  mut rows = []
  for t in $tools {
    let vars = if $t.cached { {IETS_CACHE_DIR: $cache} } else { {} }
    mut cold = []
    mut warm = []
    for _ in 1..$rounds {
      rm -rf $cache
      $cold = ($cold | append (timed $cache $vars $t.cmd))
      $warm = ($warm | append (timed $cache $vars $t.cmd))
    }
    for pair in [[state, runs]; [cold, $cold] [warm, $warm]] {
      $rows = ($rows | append {
        tool: $t.name
        run: $pair.state
        wall: $"(median ($pair.runs | get wall) | math round --precision 2)s"
        user: $"(median ($pair.runs | get user) | math round --precision 2)s"
        sys: $"(median ($pair.runs | get sys) | math round --precision 2)s"
        "max RSS": $"(median ($pair.runs | get rss) | math round --precision 0) MiB"
      })
    }
  }
  rm -rf $cache
  print ($rows | to md --pretty)
  if $no_parity or not ($nej | path exists) { return }
  if not (parity $iets $nej $args) { exit 1 }
}
