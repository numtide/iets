#!/usr/bin/env nu
# Usage: bench/perf.nu [--runs N] [--label L] [--skip-outpaths] [IETS]
# Wall, user, sys and peak RSS of one binary: nixos-desktop at 1 and 4
# workers (the run with the median wall of N, default 5), and outpaths
# chunk 0 of 8 at 1 and 4 workers (one run each, about 14 GB). Run it on an
# idle box.

def measure [iets: string, args: list<string>]: nothing -> record {
  let r = (^/usr/bin/env time -f "TIME %e %U %S %M" $iets ...$args | complete)
  let line = ($r.stderr | lines | where $it starts-with "TIME " | last)
  let f = ($line | split row " ")
  {
    wall: ($f | get 1 | into float)
    user: ($f | get 2 | into float)
    sys: ($f | get 3 | into float)
    rss_mb: (($f | get 4 | into int) / 1024 | math round)
    rc: $r.exit_code
  }
}

def median-run [iets: string, args: list<string>, runs: int]: nothing -> record {
  let rs = (1..$runs | each {|_| measure $iets $args })
  let sorted = ($rs | sort-by wall)
  $sorted | get (($runs - 1) // 2)
}

def main [
  --runs: int = 5
  --label: string = ""
  --skip-outpaths
  iets: string = "./target/release/iets"
] {
  cd ($env.FILE_PWD | path join "..")
  let np = ($env.NIXPKGS? | default "/nix/store/ah3pxz4xa52hd6vhbx17vcsci0b8bmbr-source")
  let label = if $label == "" { (^git rev-parse --short HEAD | str trim) } else { $label }
  let desktop = $"($env.PWD)/bench/workloads/nixos-desktop.nix"
  let outpaths = (mktemp --tmpdir outpaths.XXXXXX)
  $"import ($np)/ci/eval/outpaths.nix { path = ($np); systems = [ \"x86_64-linux\" ]; includeBroken = false; }\n" | save -f $outpaths
  mut rows = []
  for w in [1 4] {
    let r = (median-run $iets [eval --impure --strict --no-tracing-cache --workers ($w | into string) -I $"nixpkgs=($np)" $desktop] $runs)
    $rows = ($rows | append ($r | insert workload "nixos-desktop" | insert workers $w))
  }
  if not $skip_outpaths {
    for w in [1 4] {
      let r = (measure $iets [eval-jobs --impure --no-instantiate --workers ($w | into string) --chunks 8 --chunk 0 -I $"nixpkgs=($np)" $outpaths])
      $rows = ($rows | append ($r | insert workload "outpaths chunk 0/8" | insert workers $w))
    }
  }
  rm -f $outpaths
  print "| build | workload | workers | wall s | user s | sys s | peak RSS MB |"
  print "|---|---|---|---|---|---|---|"
  for r in $rows {
    print $"| ($label) | ($r.workload) | ($r.workers) | ($r.wall) | ($r.user) | ($r.sys) | ($r.rss_mb) |"
  }
}
