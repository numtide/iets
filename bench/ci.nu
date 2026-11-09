#!/usr/bin/env nu
# Usage: bench/ci.nu --baseline BIN --candidate BIN [--tier quick|scale|full]
#                    [--only REGEX] [--out DIR] [--threshold PCT]
#                    [--privileged [--frame-pointers] [--no-profile]]
#
# Performance regression suite: the ways a CI pipeline runs iets (job sets,
# NixOS configurations, cached reruns, small steps), each measured for two
# binaries interleaved run by run, in several machine states. The states
# are what made earlier measurements disagree between sessions: whether the
# kernel hands out huge pages, how many CPUs a runner has, a memory ceiling
# near the peak, and whether the sources are in the page cache.
#
# Active benchmarking: every run also records what limited
# it, so a number comes with its evidence: CPU time on and off the CPU, the
# run-queue wait of every thread, page faults, context switches, the
# machine's pressure stall time and its huge-page counters. The report says
# the state each result was measured in, flags noisy scenarios and matches
# the evidence against iets's known performance issues.
#
# A scenario repeats until the interval of the change is narrow enough: a
# distribution-free interval for the median of the per-round ratios, whose
# coverage the report gives (94 % at 5 rounds, 97 % at 6). Writes
# OUT/results.json and OUT/report.md and exits 1 when a scenario got slower
# (or used more memory) by more than the threshold with its whole
# interval. The same binary twice is an A/A run: it measures
# this machine's noise, which says how small a change the suite can see.
#
# --privileged (Linux, sudo) adds what needs root, and puts every setting
# back afterwards: huge pages truly scarce (compaction stopped, free memory
# fragmented, whole 2 MB blocks reserved), and per scenario a profile of
# both binaries: perf counters, on-CPU and off-CPU stacks folded for flame
# graphs. --frame-pointers says the binaries were built with
# RUSTFLAGS="-C force-frame-pointers=yes", for full stacks at 999 Hz.
#
# Wall time ends when the binary exits, as its caller sees it; a binary that
# exits early and frees its heap in a child (#370) gets that. The helper
# then waits for the child (as its subreaper), counts its CPU time, and
# only then starts the next run. Every run's output is hashed: a candidate
# that prints something else (evaluates less, say) is reported, not faster.
#
# NIXPKGS names the nixpkgs tree; by default the flake's locked input.
# darwin runs the plenty state only: it has no huge-page, CPU-pinning,
# memory-ceiling or page-cache controls.

use ../scripts/lib.nu [ROOT]
use ci/privileged.nu

const HELPER_SRC = (path self | path dirname | path join ci helper.rs)

def os []: nothing -> string { $nu.os-info.name }

# Whether an error is Ctrl-C or SIGTERM: ours from an interrupted helper, or
# nushell's own. After an interrupt nushell fails every further statement,
# so a catch rethrows first, before any cleanup; the root guard puts the
# settings back once this process is gone.
def is-interrupt [e: record]: nothing -> bool {
  $e.msg in ["interrupted" "Operation interrupted"]
}

# The helper binary, built from bench/ci/helper.rs on every invocation.
def helper [work: string]: nothing -> string {
  let bin = ($work | path join helper)
  let r = (^rustc -O --edition 2024 $HELPER_SRC -o $bin | complete)
  if $r.exit_code != 0 { error make {msg: $"building the helper failed:\n($r.stderr)"} }
  $bin
}

# ---- machine state ---------------------------------------------------------

def vmstat []: nothing -> record {
  if (os) != "linux" { return {} }
  open /proc/vmstat | lines | split column " " k v
  | where k in [thp_fault_alloc thp_fault_fallback]
  | reduce -f {} {|r, acc| $acc | insert $r.k ($r.v | into int) }
}

def read-or [path: string, fallback: string = "?"]: nothing -> string {
  try { open --raw $path | str trim } catch { $fallback }
}

# The machine and its static configuration, with the settings known to add
# noise or bias (the benchmarking checklist).
def machine []: nothing -> record {
  if (os) == "linux" {
    let sel = {|p| read-or $p | parse -r '\[(?<v>[^\]]+)\]' | get -o 0.v | default (read-or $p) }
    let m = {
      os: (^uname -srm | str trim)
      cpu: (open /proc/cpuinfo | lines | where $it starts-with "model name" | get -o 0 | default "" | split row ": " | last)
      cpus: (^nproc | into int)
      mem_gib: (open /proc/meminfo | lines | where $it starts-with "MemTotal" | get 0 | parse -r '(?<kb>\d+)' | get 0.kb | into int | $in / 1048576 | math round --precision 1)
      thp_enabled: (do $sel /sys/kernel/mm/transparent_hugepage/enabled)
      thp_defrag: (do $sel /sys/kernel/mm/transparent_hugepage/defrag)
      # -1 where the kernel has no THP (or a container hides it)
      free_2m_blocks: (try { privileged free-huge-blocks } catch { -1 })
      hpage_bytes: (read-or /sys/kernel/mm/transparent_hugepage/hpage_pmd_size "0" | into int)
      governor: (read-or /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor)
      boost: (read-or /sys/devices/system/cpu/cpufreq/boost)
      smt: (read-or /sys/devices/system/cpu/smt/active)
      sched_ext: (if (read-or /sys/kernel/sched_ext/state "") == "enabled" { read-or /sys/kernel/sched_ext/root/ops } else { "off" })
      swap_gib: (open /proc/meminfo | lines | where $it starts-with "SwapTotal" | get 0 | parse -r '(?<kb>\d+)' | get 0.kb | into int | $in / 1048576 | math round)
    }
    let warn = ([
      (if $m.governor not-in [performance "?"] { $"CPU governor ($m.governor): frequency changes add variance; `performance` is steadier" })
      (if $m.boost == "1" { "boost on: clocks follow temperature, so long runs drift" })
      (if $m.sched_ext != "off" { $"sched_ext scheduler ($m.sched_ext) loaded: it decides placement and run-queue wait, not the kernel's default" })
      (if $m.thp_enabled == "never" { "transparent huge pages off: every result is the scarce state" })
      (if $m.free_2m_blocks >= 0 and $m.free_2m_blocks * $m.hpage_bytes < 1073741824 { $"only ($m.free_2m_blocks) free huge-page blocks: huge pages are scarce now, so the plenty state is not plenty" })
    ] | compact)
    $m | insert warnings $warn
  } else {
    {
      os: $"(^sw_vers -productName | str trim) (^sw_vers -productVersion | str trim) (^uname -m | str trim)"
      cpu: (^sysctl -n machdep.cpu.brand_string | str trim)
      cpus: (^sysctl -n hw.ncpu | into int)
      mem_gib: (^sysctl -n hw.memsize | into int | $in / 1073741824 | math round --precision 1)
      warnings: []
    }
  }
}

# Waits until nothing else runs: at most 2 runnable tasks (this script and
# one other) in three samples over 0.3 s; up to `max_s` seconds.
def wait-idle [max_s: int]: nothing -> record {
  mut worst = 0
  for _ in 1..([($max_s * 10 / 3) 1] | math max) {
    let samples = (1..3 | each {
      sleep 100ms
      if (os) == "linux" {
        open /proc/stat | lines | where $it starts-with "procs_running" | get 0 | split row " " | get 1 | into int
      } else {
        # darwin has no run-queue count: busy CPUs from ps, in whole cores
        ^ps -A -o %cpu= | lines | each { str trim | into float } | math sum | $in / 100 | math round | into int | $in + 1
      }
    })
    $worst = ($samples | math max)
    if $worst <= 2 { return {worst: $worst, timed_out: false} }
  }
  {worst: $worst, timed_out: true}
}

# Who else uses the machine: busy CPUs over two seconds besides this
# script, the load average, and the heaviest processes. Waits up to ten
# minutes for a quiet machine; without --allow-busy it gives up then.
def preflight [allow_busy: bool]: nothing -> record {
  let sample = {||
    if (os) == "linux" {
      let a = (open /proc/stat | lines | first | split row -r '\s+' | skip 1 | into int)
      sleep 2sec
      let b = (open /proc/stat | lines | first | split row -r '\s+' | skip 1 | into int)
      let d = ($b | zip $a | each {|p| $p.0 - $p.1 })
      # user, nice, system and steal, as the helper counts a machine's busy
      # time; interrupt time belongs to no process
      (($d.0 + $d.1 + $d.2 + ($d | get -o 7 | default 0)) / 100 / 2)
    } else {
      sleep 2sec
      ^ps -A -o %cpu= | lines | each { str trim | into float } | math sum | $in / 100
    }
  }
  # the heaviest processes now: CPU ticks over a second, from /proc (ps's
  # %cpu is the average over a process's whole life)
  let top = {||
    if (os) == "linux" {
      let snap = {|| ls /proc | where name =~ '^/proc/\d+$' | each {|p| try { let s = (open --raw ($p.name | path join stat)); let c = ($s | parse -r '\((?<comm>.*)\) (?<rest>.*)' | get 0); let f = ($c.rest | split row " "); {pid: ($p.name | path basename), cmd: $c.comm, t: (($f | get 11 | into int) + ($f | get 12 | into int))} } catch { null } } | compact }
      let a = (do $snap)
      sleep 1sec
      let b = (do $snap)
      $b | join $a pid --left | each {|r| {cmd: $r.cmd, cpu: (($r.t - ($r.t_ | default $r.t)) | into float)} } | where cpu > 0 | sort-by cpu --reverse | first 5
    } else {
      ^ps -A -o pcpu=,comm= | lines | each { str trim | parse -r '^(?<cpu>[\d.]+)\s+(?<cmd>.*)$' | get 0 } | update cpu { into float } | sort-by cpu --reverse | first 5
    }
  }
  mut busy = (do $sample)
  mut waited = 0
  while $busy > 0.5 and $waited < 300 {
    if $waited == 0 { print $"the machine is busy: ($busy | math round --precision 1) CPUs in use; waiting for it to calm down" }
    $busy = (do $sample)
    $waited += 1
  }
  let load = if (os) == "linux" { open /proc/loadavg | split row " " | first 3 | str join " " } else { ^sysctl -n vm.loadavg | str trim }
  let r = {busy_cpus: ($busy | math round --precision 2), load: $load, top: (do $top), waited_s: ($waited * 2)}
  if $busy > 0.5 {
    let msg = $"the machine stayed busy for ten minutes: ($r.busy_cpus) CPUs in use by: ($r.top | each {|t| $'($t.cmd) ($t.cpu)%' } | str join ', ')"
    if not $allow_busy { error make {msg: $"($msg). Stop that load, or pass --allow-busy to measure anyway."} }
    print $"warning: ($msg)"
  }
  $r
}

# ---- scenarios ---------------------------------------------------------------

# Placeholders in args: {np} nixpkgs, {npw} a writable copy, {wl} the
# workloads, {work} the work directory.
def scenarios []: nothing -> list<record> {
  let rs = '{ outPath = "{np}"; revCount = 1; shortRev = "abcdefg"; }'
  let nixos = {|which, w, cache| [eval --impure --strict $cache --workers ($w | into string) -I "nixpkgs={np}" $"{wl}/nixos-($which).nix"] }
  let strict = {|w| [eval --impure --strict --json --no-cache --workers ($w | into string) "{work}/outpaths-strict.nix" --arg nixpkgs "{np}"] }
  let jobs = {|w| [eval-jobs --impure --no-instantiate --no-cache --max-memory-size "0" --workers ($w | into string) -I "nixpkgs={np}" "{work}/outpaths.nix"] }
  [
    # tier quick: the most information per second. One-worker runs of
    # mid-size evaluations resolve a 1 % change in 3 rounds of about a
    # second; parallel runs are noisier, short ones need many rounds.
    {id: "small/startup", task: "small steps", tier: "quick", cache: "cold",
     args: [eval --no-cache --expr "1"], states: [plenty]}
    {id: "small/hello-drvpath", task: "small steps", tier: "quick", cache: "cold",
     args: [eval --impure --no-cache --expr "(import {np} {}).hello.drvPath"], states: [plenty cold-files]}
    {id: "small/hello-drvpath-hit", task: "small steps", tier: "quick", cache: "warm",
     args: [eval --impure --expr "(import {np} {}).hello.drvPath"], states: [plenty]}
    {id: "nixos/minimal-w1", task: "NixOS configs", tier: "quick", cache: "cold",
     args: (do $nixos minimal 1 "--no-cache"), states: [plenty]}
    {id: "nixos/server-w1", task: "NixOS configs", tier: "quick", cache: "cold",
     args: (do $nixos server 1 "--no-cache"), states: [plenty thp-off cpus-2 cold-files]}
    {id: "nixos/server-w1-warm-ir", task: "NixOS configs", tier: "quick", cache: "warm",
     args: (do $nixos server 1 "--no-tracing-cache"), states: [plenty]}
    {id: "nixos/plasma-w1", task: "NixOS configs", tier: "quick", cache: "cold",
     args: (do $nixos plasma 1 "--no-cache"), states: [plenty thp-off]}
    # four workers: the parallel evaluator, and with huge pages off the
    # cheapest view of the page-fault costs whole nixpkgs shows
    {id: "nixos/plasma-w4", task: "NixOS configs", tier: "quick", cache: "cold",
     args: (do $nixos plasma 4 "--no-cache"), states: [plenty thp-off scarce cpus-4]}
    {id: "nixos/plasma-w8", task: "NixOS configs", tier: "quick", cache: "cold",
     args: (do $nixos plasma 8 "--no-cache"), states: [plenty]}
    {id: "rerun/server-hit", task: "cached reruns", tier: "quick", cache: "warm",
     args: [eval --impure --strict --workers "1" -I "nixpkgs={np}" "{wl}/nixos-server.nix"], states: [plenty]}
    {id: "rerun/server-one-file", task: "cached reruns", tier: "quick", cache: "edit",
     args: [eval --impure --strict --workers "1" -I "nixpkgs={npw}" "{wl}/nixos-server.nix"], states: [plenty]}
    # release-small as eval-jobs walks it by default: the aggregate job
    # `tested` alone (its other sets carry no recurseForDerivations)
    {id: "jobset/release-small", task: "job sets", tier: "quick", cache: "cold",
     args: [eval-jobs --impure --no-instantiate --no-cache --workers "4" -I "nixpkgs={np}" --arg nixpkgs $rs "{np}/nixos/release-small.nix"], states: [plenty cpus-4]}
    {id: "jobset/release-small-hit", task: "job sets", tier: "quick", cache: "warm",
     args: [eval-jobs --impure --no-instantiate --workers "4" -I "nixpkgs={np}" --arg nixpkgs $rs "{np}/nixos/release-small.nix"], states: [plenty]}
    # tier scale: whole nixpkgs, where heap-size effects (huge pages, TLB,
    # memory) show that a 1 GB evaluation hides. Quiet enough that 3
    # rounds resolve 1 %.
    {id: "nixpkgs/strict-w1", task: "whole nixpkgs", tier: "scale", cache: "cold",
     args: (do $strict 1), states: [plenty thp-off]}
    {id: "nixpkgs/strict-w8", task: "whole nixpkgs", tier: "scale", cache: "cold",
     args: (do $strict 8), states: [plenty]}
    {id: "nixpkgs/eval-jobs-w8", task: "whole nixpkgs", tier: "scale", cache: "cold",
     args: (do $jobs 8), states: [plenty]}
    # tier full: the nightly matrix, to find new problems
    {id: "nixpkgs/strict-w4", task: "whole nixpkgs", tier: "full", cache: "cold",
     args: (do $strict 4), states: [plenty thp-off scarce cpus-4]}
    {id: "nixpkgs/eval-jobs-processes", task: "whole nixpkgs", tier: "full", cache: "cold",
     args: [eval-jobs --impure --no-instantiate --no-cache -I "nixpkgs={np}" "{work}/outpaths.nix"], states: [plenty thp-off scarce memcap]}
    # every release-small job (--force-recurse): 112 jobs, NixOS tests and
    # images, about 50 GB peak RSS at 4 workers
    {id: "jobset/release-small-all", task: "job sets", tier: "full", cache: "cold",
     args: [eval-jobs --impure --no-instantiate --no-cache --force-recurse --workers "4" -I "nixpkgs={np}" --arg nixpkgs $rs "{np}/nixos/release-small.nix"], states: [plenty]}
    {id: "nixos/plasma-default-workers", task: "NixOS configs", tier: "full", cache: "cold",
     args: [eval --impure --strict --no-cache -I "nixpkgs={np}" "{wl}/nixos-plasma.nix"], states: [plenty cpus-2 cpus-4 memcap cold-files]}
  ]
}

def state-flags [state: string]: nothing -> list<string> {
  match $state {
    "thp-off" => [--thp-off]
    # the workload first for the OOM killer, not the desktop: the
    # fragmenter leaves the run little memory
    "scarce" => [--oom-first]
    "cpus-2" => [--cpus "2"]
    "cpus-4" => [--cpus "4"]
    _ => []
  }
}

def state-supported [state: string, priv: bool, np: string]: nothing -> bool {
  if $state == "plenty" { return true }
  if (os) != "linux" { return false }
  if $state == "scarce" { return $priv }
  # as many CPUs as the name says, or not at all
  if $state starts-with "cpus-" { return ((^nproc | into int) >= ($state | str replace "cpus-" "" | into int)) }
  # ZFS keeps files in its own cache, which the page-cache advice misses
  if $state == "cold-files" { return ((^stat -f -c %T $np | str trim) != "zfs") }
  true
}

def fill [args: list<string>, ctx: record]: nothing -> list<string> {
  $args | each {|a|
    $a | str replace -a "{npw}" $ctx.npw | str replace -a "{np}" $ctx.np | str replace -a "{wl}" $ctx.wl | str replace -a "{work}" $ctx.work
  }
}

# ---- running -------------------------------------------------------------------

# A command's arguments with its cache stores in `cache`: on the command line,
# which wins over IETS_CACHE_DIR and a cache-dir setting the caller may
# have, so cold runs stay cold and the two binaries never share a cache.
export def with-cache [args: list<string>, cache: string]: nothing -> list<string> {
  [($args | first) --cache-dir $cache ...($args | skip 1)]
}

# The original of the file the edit scenarios change, from the read-only
# nixpkgs; writing it back undoes any edit, also one an earlier,
# interrupted run left in the writable copy.
const EDIT_FILE = "nixos/modules/misc/version.nix"
def edit-reset [ctx: record]: nothing -> nothing {
  open --raw ($ctx.np | path join $EDIT_FILE) | save -f ($ctx.npw | path join $EDIT_FILE)
}

def run-once [ctx: record, bin: string, args: list<string>, cache: string, state: string, prefix: list<string> = []]: nothing -> record {
  if $state == "cold-files" {
    let ev = (^$ctx.helper evict $ctx.np | complete)
    if $ev.exit_code != 0 { error make {msg: $"evicting nixpkgs failed: ($ev.stderr)"} }
    let e = ($ev.stdout | from json)
    # a state that did nothing must not be reported as cold; whether the
    # advice took shows in the run's disk reads (summarize)
    if $e.files == 0 { error make {msg: $"cold-files: no file to evict under ($ctx.np)"} }
  }
  # up to ten minutes for a quiet machine; after one wait in vain, or with
  # --allow-busy, only briefly, so a busy desktop does not stall every run
  let busy_mark = ($ctx.work | path join busy)
  let max_s = if $ctx.allow_busy { 5 } else if ($busy_mark | path exists) { 30 } else { 600 }
  let w = (wait-idle $max_s)
  if $w.timed_out {
    if not ($busy_mark | path exists) { print $"warning: the machine stayed busy for ($max_s) s; later runs wait at most 30 s" }
    "" | save -f $busy_mark
  }
  let idle = $w.worst
  let err = ($ctx.work | path join "last.stderr")
  let before = (vmstat)
  let r = (with-env {XDG_CACHE_HOME: $cache, IETS_PROGRESS: "false"} {
    ^$ctx.helper run ...(state-flags $state) --stderr $err -- ...$prefix $bin ...(with-cache $args $cache) | complete
  })
  let after = (vmstat)
  # Ctrl-C or a cancelled job reaches the helper too: stop the suite, not
  # just this scenario
  if $r.exit_code in [130 143 -2 -15] { error make {msg: "interrupted"} }
  if $r.exit_code != 0 { error make {msg: $"the helper failed: ($r.stderr)"} }
  let m = ($r.stdout | from json)
  let thp = if ($before | is-empty) { {} } else {
    {thp_alloc: ($after.thp_fault_alloc - $before.thp_fault_alloc), thp_fallback: ($after.thp_fault_fallback - $before.thp_fault_fallback)}
  }
  # a run that left processes running ten minutes after it exited (the
  # helper killed them), or whose output pipe some other process kept
  # open, did not finish its work like the others did
  let orphans = ($m.orphans_killed? | default 0)
  let unhashed = ($m.out_hash? == "unknown")
  let failed = $m.exit != 0 or $m.signal != 0 or $orphans > 0 or $unhashed
  let tail = if $m.exit != 0 or $m.signal != 0 {
    open --raw $err | lines | last 5 | str join "\n"
  } else if $orphans > 0 {
    $"($orphans) processes still ran (fmt-s $m.teardown_s) after the binary exited, and were killed"
  } else if $unhashed {
    "the output pipe stayed open after every process of the run was gone"
  } else { "" }
  $m | merge $thp | merge {idle: $idle, failed: $failed, stderr_tail: $tail}
}

def median [xs: list]: nothing -> float {
  let s = ($xs | each { into float } | sort)
  let n = ($s | length)
  if $n mod 2 == 1 { $s | get ($n // 2) } else { (($s | get ($n // 2 - 1)) + ($s | get ($n // 2))) / 2 }
}

# Robust coefficient of variation in %: 1.4826 x the median absolute
# deviation over the median, so one slow run does not make a scenario
# noisy (the runs are all in results.json).
def cov [xs: list]: nothing -> float {
  let m = (median $xs)
  if $m == 0 { return 0.0 }
  let mad = (median ($xs | each {|x| ($x - $m) | math abs }))
  1.4826 * $mad / $m * 100
}

# Which order statistics bound the median of n ratios: the largest k whose
# coverage 1 - 2 P(Bin(n, 1/2) < k) is at least 95 %, else k = 1 and the
# coverage it has (75 % at 3 rounds, 94 % at 5, 97 % at 6). k grows at 9,
# 12, 15 rounds; in between a round can only widen the interval.
def ci-k [n: int]: nothing -> record {
  if $n < 2 { return {k: 0, coverage: 0.0} }
  # P(Bin(n, 1/2) <= j) for j = 0..n
  let pmf = (0..$n | each {|i| (1..([$i 1] | math max) | reduce -f 1.0 {|j, a| if $i == 0 { 1.0 } else { $a * ($n - $j + 1) / $j } }) / (2.0 ** $n) })
  let cdf = (0..$n | each {|j| $pmf | first ($j + 1) | math sum })
  let ks = (1..($n // 2) | where {|k| (1 - 2 * ($cdf | get ($k - 1))) >= 0.95 })
  let k = if ($ks | is-empty) { 1 } else { $ks | math max }
  {k: $k, coverage: (1 - 2 * ($cdf | get ($k - 1)))}
}

# The median of per-round ratios and a distribution-free interval for it:
# the k-th smallest and k-th largest ratio.
def median-ci [xs: list]: nothing -> record {
  let s = ($xs | each { into float } | sort)
  let n = ($s | length)
  if $n < 2 { return {median: (median $s), lo: 0.0, hi: 99.0, coverage: 0.0} }
  let c = (ci-k $n)
  {median: (median $s), lo: ($s | get ($c.k - 1)), hi: ($s | get ($n - $c.k)), coverage: $c.coverage}
}

def measure [ctx: record, sc: record, state: string, opts: record]: nothing -> record {
  let args = (fill $sc.args $ctx)
  let bins = [[role bin]; [baseline $ctx.baseline] [candidate $ctx.candidate]]
  # the caches warm up on the original file
  if $sc.cache == "edit" { edit-reset $ctx }
  # warm caches: one untimed run per binary, kept for that binary's runs
  let caches = ($bins | each {|b|
    let d = (^mktemp -d -p ($ctx.work | path join caches) | str trim)
    if $sc.cache in [warm edit] { run-once $ctx $b.bin $args $d "plenty" | ignore }
    {role: $b.role, dir: $d}
  })
  # one untimed run per binary first: the first run of a state pays for
  # the page cache (the scarce state drops it) and, there, for reclaim
  if $state != "cold-files" and $sc.cache == "cold" {
    for b in $bins { run-once $ctx $b.bin $args (^mktemp -d -p ($ctx.work | path join caches) | str trim) $state | ignore }
  }
  let edit_file = ($ctx.npw | path join $EDIT_FILE)
  let edit_orig = if $sc.cache == "edit" { open --raw ($ctx.np | path join $EDIT_FILE) } else { "" }
  mut runs = []
  mut round = 0
  mut ci = {median: 1.0, lo: 0.0, hi: 2.0, coverage: 0.0}
  loop {
    let order = if $round mod 2 == 0 { $bins } else { $bins | reverse }
    for b in $order {
      let cache = if $sc.cache == "cold" {
        ^mktemp -d -p ($ctx.work | path join caches) | str trim
      } else {
        $caches | where role == $b.role | get 0.dir
      }
      if $sc.cache == "edit" {
        # a change in a file every NixOS evaluation reads, the same for
        # both binaries in a round, so their output compares
        $"($edit_orig)\n# ci.nu edit ($round)\n" | save -f $edit_file
      }
      let r = (run-once $ctx $b.bin $args $cache $state)
      $runs = ($runs | append ($r | merge {role: $b.role, round: $round}))
      if $r.failed {
        print $"  ($b.role) failed: ($r.stderr_tail)"
        if $sc.cache == "edit" { edit-reset $ctx }
        return {failed: true, runs: $runs}
      }
    }
    $round += 1
    let base = ($runs | where role == baseline)
    let cand = ($runs | where role == candidate)
    let ratios = (0..($round - 1) | each {|i| ($cand | where round == $i | get 0.wall_s) / ($base | where round == $i | get 0.wall_s) })
    let heavy = (median ($base | get wall_s)) > 5.0
    let min = if $heavy { $opts.min_heavy } else { $opts.min_light }
    let max = if $heavy { $opts.max_heavy } else { $opts.max_light }
    $ci = (median-ci $ratios)
    if $round >= $max { break }
    # narrow enough, unless it still straddles a threshold, which more
    # rounds can settle; decided at the minimum and where k grows (9, 12,
    # 15 rounds), as a round in between can only widen the interval
    let t = $opts.threshold / 100
    let straddles = ($ci.lo < (1 + $t) and $ci.hi > (1 + $t)) or ($ci.lo < (1 - $t) and $ci.hi > (1 - $t))
    let decides = $round == $min or (ci-k $round).k > (ci-k ($round - 1)).k
    if $round >= $min and $decides and (($ci.hi - $ci.lo) / 2) <= ($opts.target / 100) and not $straddles { break }
  }
  if $sc.cache == "edit" { edit-reset $ctx }
  let base = ($runs | where role == baseline)
  let cand = ($runs | where role == candidate)
  let mem = (median-ci (0..($round - 1) | each {|i| ($cand | where round == $i | get 0.rss_peak_bytes) / ($base | where round == $i | get 0.rss_peak_bytes) }))
  {failed: false, runs: $runs, ci: $ci, mem_ci: $mem}
}

# Whether `systemd-run --user` can set a memory ceiling here: a scope of
# 64 MB must stop a shell that fills 300 MB, which runs without it. No user
# systemd, or no memory controller delegated to it, and the check is off.
def memcap-works []: nothing -> bool {
  let fill = "x=$(head -c 300000000 /dev/zero | tr '\\000' a); echo ${#x}"
  let free = (^sh -c $fill | complete)
  let capped = (^systemd-run --user --scope -q -p MemoryMax=64M -p MemorySwapMax=0 -- sh -c $fill | complete)
  $free.exit_code == 0 and $capped.exit_code != 0
}

# A memory ceiling of the baseline's own cgroup peak plus 10 %: does each
# binary still finish? Small CI runners kill a job at its cgroup limit. The
# peak is what the cgroup charged (shared pages once, page cache included),
# read from the scope's memory.peak, not the summed RSS of its processes.
def memcap [ctx: record, sc: record]: nothing -> record {
  let args = (fill $sc.args $ctx)
  let peakfile = ($ctx.work | path join memcap.peak)
  let d = (^mktemp -d -p ($ctx.work | path join caches) | str trim)
  # the scope's peak covers a child that frees the heap after an early exit
  let r = (with-env {XDG_CACHE_HOME: $d, IETS_PROGRESS: "false"} {
    ^systemd-run --user --scope -q -- sh -c 'f=$1; shift; "$@" >/dev/null 2>&1; rc=$?; cat "/sys/fs/cgroup$(sed -n "s/^0:://p" /proc/self/cgroup)/memory.peak" > "$f"; exit $rc' sh $peakfile $ctx.baseline ...(with-cache $args $d) | complete
  })
  if $r.exit_code != 0 or not ($peakfile | path exists) { return {available: false, why: "the baseline did not run in a user scope"} }
  let peak = (try { open --raw $peakfile | str trim | into int } catch { return {available: false, why: "memory.peak is unreadable (cgroup v2 before Linux 5.19?)"} })
  let cap = ($peak * 1.10 | math round | into int)
  let prefix = [systemd-run --user --scope -q -p $"MemoryMax=($cap)" -p "MemorySwapMax=0" --]
  # twice each: a parallel evaluation's peak varies, and one run over the
  # line is not a verdict
  let res = (1..2 | each { [[role bin]; [baseline $ctx.baseline] [candidate $ctx.candidate]] } | flatten | each {|b|
    let d = (^mktemp -d -p ($ctx.work | path join caches) | str trim)
    let r = (run-once $ctx $b.bin $args $d "plenty" $prefix)
    {role: $b.role, ok: (not $r.failed)}
  })
  let bok = ($res | where {|x| $x.role == "baseline" and $x.ok } | length)
  let cok = ($res | where {|x| $x.role == "candidate" and $x.ok } | length)
  let verdict = if $cok == 2 { "fits" } else if $cok == 0 and $bok == 2 { "OOM" } else { "flaky" }
  {available: true, cgroup_peak: $peak, cap_bytes: $cap, baseline_ok: ($bok == 2), candidate_ok: $cok, verdict: $verdict}
}

# ---- evidence and verdicts -------------------------------------------------------

def fmt-s [x: float]: nothing -> string {
  if $x < 1 { $"($x * 1000 | math round --precision 1) ms" } else { $"($x | math round --precision 2) s" }
}

def fmt-gb [x: float]: nothing -> string { $"($x / 1e9 | math round --precision 2) GB" }

def pct [x: float]: nothing -> string {
  let p = (($x - 1) * 100 | math round --precision 1)
  if $p > 0 { $"+($p) %" } else { $"($p) %" }
}

# What limited a binary's runs, from its medians.
def evidence [rs: list]: nothing -> record {
  let wall = (median ($rs | get wall_s))
  let cpu = (median ($rs | each {|r| $r.user_s + $r.sys_s }))
  let oncpu = (median ($rs | get oncpu_s? | default [0]))
  let runq = (median ($rs | get runq_s? | default [0]))
  let rss = (median ($rs | get rss_peak_bytes))
  let faults = (median ($rs | get minflt))
  let thp = if "thp_alloc" in ($rs | columns) {
    let a = ($rs | get thp_alloc | math sum)
    let f = ($rs | get thp_fallback | math sum)
    {alloc: $a, fallback: $f}
  } else { null }
  {
    wall: $wall, cov: (cov ($rs | get wall_s)), rss: $rss
    # what a child the binary left did after it exited (#370): not in the
    # wall time, which ends when the binary exits, but the next CI step
    # shares the machine with it
    teardown: (median ($rs | get teardown_s? | default [0]))
    cpu: $cpu
    # from rusage, which counts every thread to its end (the sampled
    # schedstat misses each thread's last 20 ms)
    busy_cpus: (if $wall > 0 { $cpu / $wall } else { 0 })
    runq_share: (if $oncpu > 0 { $runq / $oncpu } else { 0 })
    faults_per_gb: (if $rss > 0 { $faults / ($rss / 1e9) } else { 0 })
    major_faults: (median ($rs | get majflt))
    ivcsw: (median ($rs | get ivcsw))
    psi_mem: (median ($rs | get psi_memory_some_s? | default [0]))
    psi_cpu: (median ($rs | get psi_cpu_some_s? | default [0]))
    idle_worst: ($rs | get idle | math max)
    # user processes seen in /proc, plus what the machine total says ran
    # besides, above its rounding: processes that came and went in a run
    foreign: (if "foreign_user_cpu_s" in ($rs | columns) { $rs | each {|r| $r.foreign_user_cpu_s + (short-lived $r) } } else { [] })
    inblock_bytes: (median ($rs | get inblock? | default [0]) | $in * 512)
    foreign_kernel: ($rs | get foreign_kernel_cpu_s? | default [])
    foreign_top: ($rs | get foreign_top? | default [])
    walls: ($rs | get wall_s)
    runs: ($rs | length)
    thp: $thp
  }
}

# CPU seconds of processes that started and ended during a run, when the
# machine total shows them above its rounding.
def short-lived [r: record]: nothing -> float {
  let s = ($r.foreign_shortlived_cpu_s? | default 0.0)
  if $s > ($r.tick_floor_cpu_s? | default 0.0) { $s } else { 0.0 }
}

# iets's known performance issues, matched against a scenario's evidence:
# a "recent performance issues" checklist, so a result names its likely
# limiter instead of standing alone.
def checklist [state: string, e: record, opts: record]: nothing -> list<string> {
  [
    (if $e.cov > 3 { $"noisy: CoV ($e.cov | math round --precision 1) %; a change this small is not evidence" })
    (if $e.idle_worst > 2 { "a run started on a busy machine" })
    # other user processes' CPU during a run, per process from /proc and
    # from the machine total: more than 0.3 s and a tenth of the run's wall
    # time is a shared machine
    (if ($e.foreign | is-not-empty) and ($e.foreign | zip $e.walls | any {|p| $p.0 > 0.3 and $p.0 > 0.1 * $p.1 }) {
      let i = ($e.foreign | enumerate | sort-by item --reverse | first | get index)
      $"the measurement shared the machine: other processes used up to ($e.foreign | get $i) CPU s during a run, the most: ($e.foreign_top | get $i)" })
    (if ($e.foreign_kernel | is-not-empty) and ($e.foreign_kernel | zip $e.walls | any {|p| $p.0 > 0.3 and $p.0 > 0.1 * $p.1 }) {
      let i = ($e.foreign_kernel | enumerate | sort-by item --reverse | first | get index)
      $"kernel threads used up to ($e.foreign_kernel | get $i) CPU s during a run \(I/O, encryption or reclaim, often for this run\): ($e.foreign_top | get $i)" })
    (if $e.thp != null and $state != "scarce" and $state != "thp-off" and $e.thp.fallback > 64 * $e.runs and $e.thp.fallback > 0.01 * ($e.thp.alloc + $e.thp.fallback) {
      $"huge pages were scarce here too: ($e.thp.fallback) huge-page faults failed machine-wide; the arenas fault in 4 KB pages" })
    (if $e.thp != null and ($e.thp.alloc + $e.thp.fallback) > 0 and $state == "plenty" and $e.thp.alloc == 0 { "no huge pages at all in the plenty state" })
    (if $e.runq_share > 0.05 { $"CPU contention: threads waited ($e.runq_share * 100 | math round) % of their CPU time in the run queue \(other load, more workers than CPUs, or the scheduler\)" })
    (if $e.faults_per_gb > 100000 { $"page-fault bound: (($e.faults_per_gb / 1000) | math round) k faults per GB of peak RSS, 4 KB pages" })
    (if $e.major_faults > 100 { $"($e.major_faults) major faults: reads from disk, a cold page cache or swap" })
    (if $e.psi_mem > ($e.wall * 0.01) { $"memory pressure: tasks stalled on memory ($e.psi_mem | math round --precision 2) s" })
    (if $e.psi_cpu > ($e.wall * 0.5) and $e.runq_share <= 0.05 { "the machine's CPUs were saturated during the run" })
  ] | compact
}

def summarize [sc: record, state: string, m: record, opts: record]: nothing -> record {
  if $m.failed {
    return {scenario: $sc.id, task: $sc.task, state: $state, verdict: "FAILED", detail: ($m.runs | last | get stderr_tail), findings: []}
  }
  let base = ($m.runs | where role == baseline)
  let cand = ($m.runs | where role == candidate)
  let eb = (evidence $base)
  let ec = (evidence $cand)
  let t = $opts.threshold / 100
  # Output is deterministic by design, errors included
  # (docs/explanation/evaluation.md), so a binary whose output changes from
  # round to round has a bug. The edit scenario changes its input every
  # round, so there only the two binaries of one round compare. Otherwise
  # the same output from both, or the times compare different work.
  let nondet = if $sc.cache == "edit" { [] } else {
    [[role runs]; [baseline $base] [candidate $cand]] | where {|x| ($x.runs | get -o out_hash | uniq | length) > 1 } | get role
  }
  let same = ($base | each {|b| $b.out_hash? == ($cand | where round == $b.round | get -o 0.out_hash?) })
  let out = if ($nondet | is-not-empty) { "nondeterministic" } else if ($same | all { $in }) { "same" } else { "differs" }
  # memory too needs its whole interval past the threshold, and 20 MB, so
  # one peak of a summed process tree cannot fail a run
  # cold-files with nothing read from disk was not cold (a file system
  # that ignores the page-cache advice): no result for that state
  let not_cold = $state == "cold-files" and ($eb.inblock_bytes < 1e6 or $ec.inblock_bytes < 1e6)
  let verdict = if $out == "nondeterministic" {
    "NONDETERMINISTIC"
  } else if $out == "differs" and not $opts.expect_output_change {
    "OUTPUT DIFFERS"
  } else if $not_cold {
    "n/a"
  } else if $m.ci.lo > (1 + $t) {
    "SLOWER"
  } else if $m.mem_ci.lo > (1 + $opts.mem_threshold / 100) and ($ec.rss - $eb.rss) > 20e6 {
    "MORE MEMORY"
  } else if $m.ci.hi < (1 - $t) {
    "faster"
  } else { "no change" }
  let hp = {|e| if $e.thp == null { "n/a" } else if ($e.thp.alloc + $e.thp.fallback) == 0 { "none" } else { $"($e.thp.alloc * 100 / ($e.thp.alloc + $e.thp.fallback) | math round) %" } }
  let range = {|rs| $"[(fmt-s ($rs | get wall_s | math min))–(fmt-s ($rs | get wall_s | math max))]" }
  {
    scenario: $sc.id, task: $sc.task, state: $state, rounds: ($base | length)
    baseline: $"(fmt-s $eb.wall) (do $range $base)"
    candidate: $"(fmt-s $ec.wall) (do $range $cand)"
    change: $"(pct $m.ci.median) [(pct $m.ci.lo), (pct $m.ci.hi)] ($m.ci.coverage * 100 | math round) %"
    "peak RSS": $"(fmt-gb $eb.rss) → (fmt-gb $ec.rss)"
    "CPU s": $"(fmt-s $eb.cpu) → (fmt-s $ec.cpu)"
    # what ran on after the binary exited, which the wall time leaves out
    teardown: (if ($eb.teardown + $ec.teardown) < 0.01 { "none" } else { $"(fmt-s $eb.teardown) → (fmt-s $ec.teardown)" })
    "busy CPUs": $"($eb.busy_cpus | math round --precision 1) → ($ec.busy_cpus | math round --precision 1)"
    # sampled every 20 ms: meaningless for a run shorter than a few samples
    "run-queue": (if $eb.wall < 0.1 { "n/a" } else { $"($eb.runq_share * 100 | math round --precision 1) % → ($ec.runq_share * 100 | math round --precision 1) %" })
    "huge pages": $"(do $hp $eb) → (do $hp $ec)"
    "CoV": $"($eb.cov | math round --precision 1) % / ($ec.cov | math round --precision 1) %"
    "foreign CPU": (if ($eb.foreign | is-empty) { "n/a" } else { $"(($eb.foreign ++ $ec.foreign | math max) | math round --precision 2) s max" })
    verdict: $verdict
    detail: ([
      (if $not_cold { $"not cold: the runs read (fmt-gb $eb.inblock_bytes) / (fmt-gb $ec.inblock_bytes) from disk after the eviction" })
      (if $out == "nondeterministic" { $"the output of the ($nondet | str join ' and the ') changed from round to round \(($nondet | each {|r| $m.runs | where role == $r | get -o out_hash | uniq | length } | str join ' / ') different outputs\); the times are in the row" })
      (if $out == "differs" { $"the binaries printed different output: baseline ($base | get 0.out_lines) lines, candidate ($cand | get 0.out_lines) lines(if $opts.expect_output_change { ' (expected: --expect-output-change)' } else { '; the times are in the row' })" })
      (if ($base | get 0.cpus? | default "") != "" { $"CPUs ($base | get 0.cpus)" })
    ] | compact | str join "; ")
    findings: ((checklist $state $eb $opts) ++ (checklist $state $ec $opts) ++ (teardown-finding $eb $ec) | uniq)
  }
}

# Work moved past the binary's exit: a child that frees the heap, or
# anything else left running, which the wall time does not see but the
# next step of a CI job shares the machine with. Named when the candidate
# leaves 100 ms and 5 % of its wall time more of it than the baseline,
# with the change the two times together make.
def teardown-finding [eb: record, ec: record]: nothing -> list<string> {
  let more = $ec.teardown - $eb.teardown
  if $more <= 0.1 or $more <= 0.05 * $ec.wall { return [] }
  let both = ($ec.wall + $ec.teardown) / ($eb.wall + $eb.teardown)
  [$"the candidate leaves (fmt-s $ec.teardown) of work running after it exits \(baseline (fmt-s $eb.teardown)\), which its wall time leaves out: wall time and that together change by (pct $both)"]
}

# A finished row, appended to rows.jsonl at once: an interrupted run keeps
# every row it had.
def keep-row [out: string, row: record]: nothing -> nothing {
  $"($row | to json -r)\n" | save --append ($out | path join rows.jsonl)
}

# ---- main ------------------------------------------------------------------------

def main [
  --baseline: string    # iets binary to compare against
  --candidate: string   # iets binary under test
  --tier: string = "scale"   # quick (minutes), scale (+ whole nixpkgs, about half an hour), full (nightly, hours)
  --only: string = ""   # run only scenarios whose id matches this regex
  --out: string = ""    # result directory; default bench/results/ci/DATE-HOST
  --threshold: float = 3.0       # % wall change, beyond the interval, that fails
  --mem-threshold: float = 5.0   # % peak RSS change that fails
  --target: float = 1.0          # stop repeating at this ± % interval
  --privileged          # Linux, sudo: scarce huge pages and profiles
  --frame-pointers      # the binaries keep frame pointers: full stacks
  --no-profile          # with --privileged: skip the profiles
  --allow-busy          # measure even if the machine stays busy (reported)
  --expect-output-change  # the candidate prints different output on purpose: compare the times anyway
] {
  if $baseline == null or $candidate == null { error make {msg: "--baseline and --candidate are required"} }
  let baseline = ($baseline | path expand)
  let candidate = ($candidate | path expand)
  # every argument checked before anything is changed or started
  let tiers = match $tier { "quick" => [quick], "scale" => [quick scale], "full" => [quick scale full], _ => (error make {msg: "--tier is quick, scale or full"}) }
  # a bare number in a list literal is an int, and would only fail when its
  # scenario comes up
  for s in (scenarios) {
    let bad = ($s.args | where {|a| ($a | describe) != "string" })
    if ($bad | is-not-empty) { error make {msg: $"scenario ($s.id): arguments that are not strings: ($bad | to nuon)"} }
  }
  let todo = (scenarios | where {|s| $s.tier in $tiers and ($only == "" or $s.id =~ $only) })
  if ($todo | is-empty) { error make {msg: "no scenario matches"} }
  for b in [$baseline $candidate] { if not ($b | path exists) { error make {msg: $"($b) does not exist"} } }
  cd $ROOT
  # the caller's iets settings reach neither binary: IETS_* set here would
  # change what both runs do and stay out of the report. Recorded, with
  # the settings files that still apply alike to both.
  let cleared = ($env | columns | where $it starts-with "IETS_")
  hide-env ...$cleared
  let config = {
    cleared_env: $cleared
    nix_config_env: ($env | columns | where $it in [NIX_CONFIG NIX_CONF_DIR NIX_USER_CONF_FILES XDG_CONFIG_HOME XDG_CONFIG_DIRS] | each {|k| $"($k)=($env | get $k)" })
  }
  let np = ($env.NIXPKGS? | default (^nix eval --raw --impure --expr '(builtins.getFlake (toString ./.)).inputs.nixpkgs.outPath' | str trim))
  # the work directory under the cache, not /tmp, which may be memory
  let home = ($env.XDG_CACHE_HOME? | default ($env.HOME | path join .cache) | path join iets-ci)
  mkdir $home
  # nothing here is deleted (fresh directories instead): say when it grows
  let used = (du $home | get 0.physical | into int)
  if $used > 5e9 { print $"warning: ($home) holds (fmt-gb ($used | into float)) from earlier runs \(work-* directories, a nixpkgs copy\); clear it when no run is going" }
  let work = (^mktemp -d -p $home work-XXXXXX | str trim)
  mkdir ($work | path join caches)
  let out = if $out == "" { $ROOT | path join bench results ci $"(date now | format date '%Y-%m-%d-%H%M')-(sys host | get hostname)" } else { $out | path expand }
  mkdir $out
  $"import ($np)/ci/eval/outpaths.nix { path = ($np); systems = [ \"(^nix eval --raw --impure --expr builtins.currentSystem)\" ]; }\n" | save -f ($work | path join outpaths.nix)
  ^cp ($ROOT | path join bench workloads outpaths-strict.nix) ($work | path join outpaths-strict.nix)
  # a writable nixpkgs only for the one-file scenario, kept for the next run
  let npw = ($home | path join $"nixpkgs-($np | path basename)")
  # copied under a temporary name and renamed when whole, so an
  # interrupted copy is never taken for one
  if ($todo | any {|s| $s.cache == "edit" }) and not ($npw | path exists) {
    let tmp = (^mktemp -d -p $home $"($npw | path basename).partial-XXXXXX" | str trim)
    ^cp -r $"($np)/." $tmp
    ^chmod -R u+w $tmp
    ^mv -T $tmp $npw
  }
  let ctx = {
    baseline: $baseline, candidate: $candidate, np: $np, npw: $npw, allow_busy: $allow_busy
    wl: ($ROOT | path join bench workloads), work: $work, helper: (helper $work)
  }
  let opts = {threshold: $threshold, mem_threshold: $mem_threshold, target: $target,
              min_light: 6, max_light: 15, min_heavy: 5, max_heavy: 9,
              expect_output_change: $expect_output_change}
  # the same file content, wherever the two copies are
  let aa = (open --raw $baseline | hash sha256) == (open --raw $candidate | hash sha256)
  let mach = (machine)
  print $"machine: ($mach | reject warnings | to nuon)"
  if ($cleared | is-not-empty) { print $"cleared for both binaries: ($cleared | str join ', ')" }
  for w in $mach.warnings { print $"warning: ($w)" }
  if $aa { print "A/A run: baseline and candidate are the same binary; the changes are this machine's noise" }
  let pre = (preflight $allow_busy)
  print $"preflight: ($pre.busy_cpus) CPUs busy, load ($pre.load); top: ($pre.top | each {|t| $'($t.cmd) ($t.cpu)%' } | str join ', ')"
  let memcap_ok = if (os) == "linux" and ($todo | any {|s| "memcap" in $s.states }) { memcap-works } else { false }
  let saved = if $privileged { privileged begin $ctx.helper $work } else { null }
  # from here every exit, by error or not, goes through restore
  let result = (try {
    mut rows = []
    mut raw = []
    mut profiles = []
    for sc in $todo {
      mut peak = 0.0
      for state in ($sc.states | where $it != memcap) {
        if not (state-supported $state $privileged $np) { continue }
        print $"($sc.id) [($state)]"
        # a scenario that errors is recorded as failed, and the rest still run
        let m = (try {
          let s = if $state == "scarce" { privileged scarce-begin $ctx.helper $work $peak } else { null }
          let r = (measure $ctx $sc $state $opts)
          # a row named scarce must have been scarce to its end
          let held = if $state == "scarce" { privileged scarce-held $work $s } else { null }
          if $state == "scarce" { privileged scarce-end $saved $work }
          if $held != null and not $held.held {
            {failed: true, runs: ($r.runs | append {stderr_tail: $"huge pages were not scarce to the end: ($held.why)"}), scarce: $s}
          } else {
            $r | merge {scarce: $s}
          }
        } catch {|e|
          if (is-interrupt $e) { error make {msg: "interrupted"} }
          if $sc.cache == "edit" { try { edit-reset $ctx } }
          if $state == "scarce" { privileged scarce-end $saved $work }
          {failed: true, runs: [{stderr_tail: $"the suite failed here: ($e.msg)"}], scarce: null}
        })
        let detail = if $m.scarce != null { {detail: $"($m.scarce.whole_before) whole huge-page blocks were free, ($m.scarce.reserved) reserved, ($m.scarce.free_2m_blocks) left"} } else { {} }
        # a bug in the summary loses this row, not the run
        let row = (try { summarize $sc $state $m $opts | merge $detail } catch {|e|
          if (is-interrupt $e) { error make {msg: "interrupted"} }
          {scenario: $sc.id, task: $sc.task, state: $state, verdict: "FAILED", detail: $"summarizing failed: ($e.msg)", findings: []}
        })
        print $"  ($row.change? | default '') ($row.verdict)"
        for f in $row.findings { print $"  - ($f)" }
        $rows = ($rows | append $row)
        keep-row $out $row
        $raw = ($raw | append {scenario: $sc.id, state: $state, runs: $m.runs, ci: ($m.ci? | default null), mem_ci: ($m.mem_ci? | default null)})
        if $state == "plenty" and not $m.failed {
          $peak = (median ($m.runs | where role == baseline | get rss_peak_bytes))
          if "memcap" in $sc.states and (os) == "linux" {
            let row = try { if not $memcap_ok {
              {scenario: $sc.id, task: $sc.task, state: "memcap", verdict: "n/a", detail: "no memory ceiling: systemd-run --user cannot limit memory here (no user systemd, or no delegated memory controller)", findings: []}
            } else {
              let mc = (memcap $ctx $sc)
              if not $mc.available {
                {scenario: $sc.id, task: $sc.task, state: "memcap", verdict: "n/a", detail: $mc.why, findings: []}
              } else if not $mc.baseline_ok {
                {scenario: $sc.id, task: $sc.task, state: $"memcap (fmt-gb ($mc.cap_bytes | into float))", verdict: "n/a", detail: $"the baseline does not finish within its own cgroup peak ((fmt-gb ($mc.cgroup_peak | into float))) + 10 %", findings: []}
              } else {
                {scenario: $sc.id, task: $sc.task, state: $"memcap (fmt-gb ($mc.cap_bytes | into float))", verdict: $mc.verdict, detail: $"the baseline's cgroup peak (fmt-gb ($mc.cgroup_peak | into float)) + 10 %; candidate finished ($mc.candidate_ok) of 2 runs", findings: []}
              }
            } } catch {|e|
              if (is-interrupt $e) { error make {msg: "interrupted"} }
              {scenario: $sc.id, task: $sc.task, state: "memcap", verdict: "FAILED", detail: $"the check failed: ($e.msg)", findings: []}
            }
            print $"  ($row.state): ($row.verdict) ($row.detail)"
            $rows = ($rows | append $row)
            keep-row $out $row
          }
          if $privileged and not $no_profile {
            let args = (fill $sc.args $ctx)
            let prof = {|role, bin|
              # every profiled run in the scenario's cache state
              let d = (^mktemp -d -p ($work | path join caches) | str trim)
              if $sc.cache in [warm edit] { run-once $ctx $bin $args $d "plenty" | ignore }
              let prepare = match $sc.cache {
                "cold" => {|| ^mktemp -d -p ($work | path join caches) | str trim }
                "edit" => {|| $"(open --raw ($ctx.np | path join $EDIT_FILE))\n# ci.nu profile edit (random uuid)\n" | save -f ($ctx.npw | path join $EDIT_FILE); $d }
                _ => {|| $d }
              }
              let name = ($sc.id | str replace -a "/" "-")
              privileged profile $ctx.helper $bin $args ($out | path join profiles $name $role) ($work | path join perf $name $role) $prepare $frame_pointers
            }
            # a profile that fails is a row, not the end of the run
            let p = (try {
              {ok: {scenario: $sc.id, baseline: (do $prof baseline $baseline), candidate: (do $prof candidate $candidate)}}
            } catch {|e|
              if (is-interrupt $e) { error make {msg: "interrupted"} }
              {err: {scenario: $sc.id, task: $sc.task, state: "profile", verdict: "FAILED", detail: $"the profile failed: ($e.msg)", findings: []}}
            })
            if $sc.cache == "edit" { edit-reset $ctx }
            if "ok" in $p { $profiles = ($profiles | append $p.ok) } else { $rows = ($rows | append $p.err); keep-row $out $p.err }
          }
        }
      }
    }
    {rows: $rows, raw: $raw, profiles: $profiles}
  } catch {|e|
    # after an interrupt nothing more runs here; the guard restores
    if (is-interrupt $e) { error make {msg: $"interrupted; finished rows in ($out)/rows.jsonl"} }
    if $saved != null { privileged restore $saved $work }
    # where it failed, for a bug in the suite itself
    print ($e.rendered? | default ($e | to nuon))
    error make {msg: $"($e.msg)\nwork files: ($work)"}
  })
  if $saved != null { privileged restore $saved $work }
  let rows = $result.rows
  {machine: $mach, preflight: $pre, baseline: $baseline, candidate: $candidate, nixpkgs: $np, tier: $tier, aa: $aa,
   privileged: $privileged, saved_settings: $saved, config: $config, rows: $rows, raw: $result.raw, profiles: $result.profiles}
  | to json | save -f ($out | path join results.json)
  let table_cols = [scenario state rounds baseline candidate change "peak RSS" "CPU s" teardown "busy CPUs" "run-queue" "huge pages" CoV "foreign CPU" verdict]
  let md = ([
    $"# iets CI performance: (date now | format date '%Y-%m-%d %H:%M')"
    ""
    $"- machine: ($mach | reject warnings | items {|k, v| $'($k)=($v)' } | str join ', ')"
    ...($mach.warnings | each {|w| $"- warning: ($w)" })
    $"- baseline: `($baseline)`"
    $"- candidate: `($candidate)`(if $aa { ' (A/A: the changes below are noise)' } else { '' })"
    (if ($cleared | is-not-empty) { $"- cleared for both binaries: ($cleared | str join ', ')" } else { "- no IETS_* variables were set" })
    $"- Nix settings from the environment: (if ($config.nix_config_env | is-empty) { 'none; the usual files' } else { $config.nix_config_env | str join ', ' })"
    $"- nixpkgs: `($np)`; tier ($tier); fails when the whole interval is past +($threshold) % wall, or +($mem_threshold) % and 20 MB peak RSS"
    "- change: median of the per-round ratios [distribution-free interval] and that interval's coverage"
    $"- before the run: ($pre.busy_cpus) CPUs busy, load ($pre.load)(if $pre.waited_s > 0 { $', after waiting ($pre.waited_s) s' } else { '' }); heaviest processes: ($pre.top | each {|t| $'($t.cmd) ($t.cpu)%' } | str join ', ')"
    "- foreign CPU: CPU seconds other user processes used during a run, per process from /proc plus short-lived ones from the machine total, the largest of all runs; each run also waits until nothing else is runnable before it starts"
    "- output: every run's output is hashed, the numbers in eval-jobs `stats` aside; NONDETERMINISTIC when a binary's own output changed from round to round, OUTPUT DIFFERS when the two binaries printed different output"
    "- teardown: how long what the binary left running went on after it exited (its CPU time is in CPU s); the next run starts after it, and a run whose leftovers still go on after ten minutes is killed and FAILED"
    "- busy CPUs: CPU time / wall; run-queue: time threads waited for a CPU / on-CPU time; huge pages: share of huge-page faults that got one, counted machine-wide (/proc/vmstat), which an idle machine makes this run's"
    ""
  ] | append ($rows | group-by task | items {|task, rs|
    let table = ($rs | each {|r| $table_cols | reduce -f {} {|c, a| $a | insert $c ($r | get -o $c | default "") } } | to md)
    let notes = ($rs | where {|r| ($r.findings | is-not-empty) or ($r.detail != "") } | each {|r|
      [$"- **($r.scenario) [($r.state)]**: ($r.detail)" ...($r.findings | each {|f| $"  - ($f)" })] | str join "\n"
    })
    (["" $"## ($task)" "" $table ""] ++ $notes) | str join "\n"
  }) | append (if ($result.profiles | is-empty) { [] } else {
    ["" "## profiles (perf, one run each)" ""] ++ ($result.profiles | each {|p|
      let c = {|x, k| $x.counters | get -o $k | default 0 }
      let rate = {|x, a, b| if (do $c $x $b) > 0 { $"((do $c $x $a) * 100 / (do $c $x $b) | math round --precision 2) %" } else { "?" } }
      [$"### ($p.scenario)" ""
       $"| | baseline | candidate |" "|---|---|---|"
       $"| IPC | ($p.baseline.ipc | default 0 | math round --precision 2) | ($p.candidate.ipc | default 0 | math round --precision 2) |"
       $"| branch misses | (do $rate $p.baseline branch-misses branches) | (do $rate $p.candidate branch-misses branches) |"
       $"| cache misses | (do $rate $p.baseline cache-misses cache-references) | (do $rate $p.candidate cache-misses cache-references) |"
       $"| dTLB misses | (do $rate $p.baseline dTLB-load-misses dTLB-loads) | (do $rate $p.candidate dTLB-load-misses dTLB-loads) |"
       $"| page faults | (do $c $p.baseline page-faults) | (do $c $p.candidate page-faults) |"
       $"| context switches | (do $c $p.baseline context-switches) | (do $c $p.candidate context-switches) |"
       "" $"on-CPU, baseline: ($p.baseline.oncpu_top | each {|t| $'($t.function) ($t.share)%' } | str join ', ')"
       "" $"on-CPU, candidate: ($p.candidate.oncpu_top | each {|t| $'($t.function) ($t.share)%' } | str join ', ')"
       "" $"off-CPU time, baseline: ($p.baseline.offcpu_top | each {|t| $'($t.function) ($t.share)%' } | str join ', ')"
       "" $"off-CPU time, candidate: ($p.candidate.offcpu_top | each {|t| $'($t.function) ($t.share)%' } | str join ', ')"
       "" $"stacks: ($p.candidate.stacks); folded stacks and flame graphs under profiles/" ""] | str join "\n"
    })
  }) | str join "\n")
  $md | save -f ($out | path join report.md)
  print $md
  print $"results: ($out)"
  print $"work files left in ($work)"
  let bad = ($rows | where verdict in [SLOWER "MORE MEMORY" OOM FAILED "OUTPUT DIFFERS" NONDETERMINISTIC])
  # an A/A run fails on a crash or on output that changes from run to run,
  # not on a difference
  if ($rows | where verdict in [FAILED NONDETERMINISTIC] | is-not-empty) or (($bad | is-not-empty) and not $aa) { exit 1 }
}
