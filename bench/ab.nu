#!/usr/bin/env nu
# Usage: bench/ab.nu [--runs N] [--skip-outpaths] BEFORE AFTER
# Interleaved A/B of two iets binaries, on nixos-server at 1 and 4 workers
# and pure from a checkout at 1, nixos-minimal at 1 worker, and outpaths
# chunk 0 of 8 (`eval-jobs --no-instantiate`) at 1 worker. After one warmup
# each, N pairs (default 8, even) run in the order AB, BA, AB, ..., so a
# machine that drifts during the run moves both sides alike, and each binary
# runs second in half the pairs, so a cache the first run warms favors
# neither; hyperfine runs one command's runs and then the other's, and
# cannot. Prints the median wall,
# mean user and sys, the median peak RSS, and the change: the median of each
# pair's change, which drift does not bias as it biases two medians. Needs
# GNU time. Run it on an idle box.

# One run: wall from the clock around it (GNU time's %e has 10 ms steps),
# user, sys and peak RSS from GNU time.
def run-once [bin: string, args: list<string>]: nothing -> record {
  let t0 = (date now)
  let r = (^/usr/bin/env time -f "TIME %U %S %M" $bin ...$args | complete)
  let wall = (((date now) - $t0) | into int) / 1e9
  if $r.exit_code != 0 {
    error make {msg: $"($bin) ($args | str join ' ') exited ($r.exit_code):\n($r.stderr | lines | last 5 | str join "\n")"}
  }
  let f = ($r.stderr | lines | where $it starts-with "TIME " | last | split row " ")
  {
    wall: $wall
    user: ($f | get 1 | into float)
    sys: ($f | get 2 | into float)
    rss_mb: (($f | get 3 | into int) / 1024 | math round)
  }
}

# Everything that can fail or be interrupted once the temporary files exist,
# so `main` removes them either way.
def measure [
  before: string, after: string, runs: int, skip_outpaths: bool, np: string,
  server: string, minimal: string, outpaths: string, checkout: string
]: nothing -> list<record> {
  ^cp -r $"($np)/." $checkout
  ^chmod -R u+w $checkout
  "" | save -f $"($checkout)/iets.toml"
  open --raw $server | str replace "<nixpkgs>" "./." | save -f $"($checkout)/bench.nix"
  $"import ($np)/ci/eval/outpaths.nix { path = ($np); systems = [ \"x86_64-linux\" ]; includeBroken = false; }\n" | save -f $outpaths
  mut cases = [
    { name: "nixos-server w1", args: [eval --impure --strict --no-tracing-cache --workers "1" -I $"nixpkgs=($np)" $server] }
    { name: "nixos-server w4", args: [eval --impure --strict --no-tracing-cache --workers "4" -I $"nixpkgs=($np)" $server] }
    { name: "nixos-server pure w1", args: [eval --strict --no-tracing-cache --workers "1" --lint "absolute-path-in-drv=off" $"($checkout)/bench.nix"] }
    { name: "nixos-minimal w1", args: [eval --impure --strict --no-tracing-cache --workers "1" -I $"nixpkgs=($np)" $minimal] }
  ]
  if not $skip_outpaths {
    $cases = ($cases | append { name: "outpaths 0/8 w1", args: [eval-jobs --impure --no-instantiate --workers "1" --chunks "8" --chunk "0" -I $"nixpkgs=($np)" $outpaths] })
  }
  mut rows = []
  for c in $cases {
    run-once $before $c.args | ignore
    run-once $after $c.args | ignore
    mut a = []
    mut b = []
    for i in 0..<$runs {
      if $i mod 2 == 0 {
        $a = ($a | append (run-once $before $c.args))
        $b = ($b | append (run-once $after $c.args))
      } else {
        $b = ($b | append (run-once $after $c.args))
        $a = ($a | append (run-once $before $c.args))
      }
    }
    let ra = $a
    let rb = $b
    let change = (0..<$runs | each {|i| (($rb | get $i).wall - ($ra | get $i).wall) / ($ra | get $i).wall } | math median)
    $rows = ($rows | append {
      workload: $c.name
      before_wall: ($ra.wall | math median | math round --precision 3)
      after_wall: ($rb.wall | math median | math round --precision 3)
      change: $"(($change * 100) | math round --precision 1)%"
      before_user: ($ra.user | math avg | math round --precision 2)
      after_user: ($rb.user | math avg | math round --precision 2)
      before_sys: ($ra.sys | math avg | math round --precision 2)
      after_sys: ($rb.sys | math avg | math round --precision 2)
      before_rss: ($ra.rss_mb | math median | math round)
      after_rss: ($rb.rss_mb | math median | math round)
    })
  }
  $rows
}

def main [
  --runs: int = 8
  --skip-outpaths
  before: string
  after: string
] {
  if $runs <= 0 or $runs mod 2 != 0 {
    error make {msg: $"--runs must be even and positive, so AB and BA pairs balance: ($runs)"}
  }
  cd ($env.FILE_PWD | path join "..")
  if (^/usr/bin/env time -f "%M" true | complete).exit_code != 0 {
    error make {msg: "ab.nu needs GNU time for `time -f`"}
  }
  let np = ($env.NIXPKGS? | default "/nix/store/ah3pxz4xa52hd6vhbx17vcsci0b8bmbr-source")
  let server = $"($env.PWD)/bench/workloads/nixos-server.nix"
  let minimal = $"($env.PWD)/bench/workloads/nixos-minimal.nix"
  let outpaths = (mktemp --tmpdir outpaths.XXXXXX)
  # a checkout with a project marker: every file a pure eval reads is a
  # project path, which `store_hooks::admits` checks
  let checkout = (mktemp -d --tmpdir checkout.XXXXXX)
  let rows = try {
    measure $before $after $runs $skip_outpaths $np $server $minimal $outpaths $checkout
  } catch {|e|
    rm -rf $outpaths $checkout
    error make {msg: $e.msg}
  }
  rm -rf $outpaths $checkout
  print "| workload | wall before | wall after | change | user before | user after | sys before | sys after | RSS before | RSS after |"
  print "|---|---|---|---|---|---|---|---|---|---|"
  for r in $rows {
    print $"| ($r.workload) | ($r.before_wall) | ($r.after_wall) | ($r.change) | ($r.before_user) | ($r.after_user) | ($r.before_sys) | ($r.after_sys) | ($r.before_rss) | ($r.after_rss) |"
  }
}
