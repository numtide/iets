#!/usr/bin/env nu
# Usage: bench/ab.nu [--runs N] [--skip-outpaths] BEFORE AFTER
# Interleaved A/B of two iets binaries: one hyperfine invocation per
# workload (`--warmup 1 --runs N`, default 7) so the runs alternate, on
# nixos-desktop at 1 and 4 workers, nixos-minimal at 1 worker, and outpaths
# chunk 0 of 8 (`eval-jobs --no-instantiate`) at 1 worker. Prints the
# median wall, mean user and sys, the peak RSS of one extra timed run, and
# the wall change. Run it on an idle box.

def rss-mb [bin: string, args: list<string>]: nothing -> int {
  let r = (^/usr/bin/env time -f "TIME %M" $bin ...$args | complete)
  let line = ($r.stderr | lines | where $it starts-with "TIME " | last)
  ($line | split row " " | get 1 | into int) / 1024 | math round
}

def main [
  --runs: int = 7
  --skip-outpaths
  before: string
  after: string
] {
  cd ($env.FILE_PWD | path join "..")
  let np = ($env.NIXPKGS? | default "/nix/store/ah3pxz4xa52hd6vhbx17vcsci0b8bmbr-source")
  let desktop = $"($env.PWD)/bench/workloads/nixos-desktop.nix"
  let minimal = $"($env.PWD)/bench/workloads/nixos-minimal.nix"
  let outpaths = (mktemp --tmpdir outpaths.XXXXXX)
  $"import ($np)/ci/eval/outpaths.nix { path = ($np); systems = [ \"x86_64-linux\" ]; includeBroken = false; }\n" | save -f $outpaths
  mut cases = [
    { name: "nixos-desktop w1", args: [eval --impure --strict --no-tracing-cache --workers "1" -I $"nixpkgs=($np)" $desktop] }
    { name: "nixos-desktop w4", args: [eval --impure --strict --no-tracing-cache --workers "4" -I $"nixpkgs=($np)" $desktop] }
    { name: "nixos-minimal w1", args: [eval --impure --strict --no-tracing-cache --workers "1" -I $"nixpkgs=($np)" $minimal] }
  ]
  if not $skip_outpaths {
    $cases = ($cases | append { name: "outpaths 0/8 w1", args: [eval-jobs --impure --no-instantiate --workers "1" --chunks "8" --chunk "0" -I $"nixpkgs=($np)" $outpaths] })
  }
  mut rows = []
  for c in $cases {
    let json = (mktemp --tmpdir ab.XXXXXX)
    let a = ([$before] ++ $c.args | str join " ")
    let b = ([$after] ++ $c.args | str join " ")
    ^hyperfine -N --warmup 1 --runs $runs --export-json $json $a $b out+err> /dev/null
    let res = (open --raw $json | from json | get results)
    rm -f $json
    let ra = ($res | get 0)
    let rb = ($res | get 1)
    let rss_a = (rss-mb $before $c.args)
    let rss_b = (rss-mb $after $c.args)
    $rows = ($rows | append {
      workload: $c.name
      before_wall: ($ra.median | math round --precision 3)
      after_wall: ($rb.median | math round --precision 3)
      change: $"(((($rb.median - $ra.median) / $ra.median) * 100) | math round --precision 1)%"
      before_user: ($ra.user | math round --precision 2)
      after_user: ($rb.user | math round --precision 2)
      before_sys: ($ra.system | math round --precision 2)
      after_sys: ($rb.system | math round --precision 2)
      before_rss: $rss_a
      after_rss: $rss_b
    })
  }
  rm -f $outpaths
  print "| workload | wall before | wall after | change | user before | user after | sys before | sys after | RSS before | RSS after |"
  print "|---|---|---|---|---|---|---|---|---|---|"
  for r in $rows {
    print $"| ($r.workload) | ($r.before_wall) | ($r.after_wall) | ($r.change) | ($r.before_user) | ($r.after_user) | ($r.before_sys) | ($r.after_sys) | ($r.before_rss) | ($r.after_rss) |"
  }
}
