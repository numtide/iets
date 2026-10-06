#!/usr/bin/env nu
# Usage: bench/cache-growth.nu
# What the two caches cost on disk: nixos-desktop at the pinned nixpkgs, then
# the same workload at a second nixpkgs revision in the store. The second row
# is what one revision of drift adds, so it names the dedup ratio.

def sizes [cache: path] {
  let ir = ($cache | path join "ir")
  let trace_entries = (ls $cache | where type == dir | where ($it.name | path basename) != "ir" | length)
  let ir_entries = (if ($ir | path exists) { glob $"($ir)/*/*" | length } else { 0 })
  let ir_bytes = (if ($ir | path exists) { ^du -sb $ir | split row "\t" | get 0 | into int } else { 0 })
  let all_bytes = (^du -sb $cache | split row "\t" | get 0 | into int)
  {
    trace_entries: $trace_entries
    trace_bytes: ($all_bytes - $ir_bytes)
    ir_entries: $ir_entries
    ir_bytes: $ir_bytes
  }
}

def mib [n: int] { $"(($n / 1048576) | math round --precision 1) MiB" }

def count_delta [now: int, before: int] { $"($now) \(+($now - $before)\)" }

def size_delta [now: int, before: int] { $"(mib $now) \(+(mib ($now - $before))\)" }

def main [] {
  cd ($env.FILE_PWD | path join "..")
  let iets = ($env.IETS? | default $"($env.PWD)/target/release/iets")
  let np1 = ($env.NIXPKGS? | default "/nix/store/ah3pxz4xa52hd6vhbx17vcsci0b8bmbr-source")
  let np2 = ($env.NIXPKGS2? | default "/nix/store/bgp6cqzszs95fdsrjsl6gpy540rjrac9-source")
  let cache = $"($env.TMPDIR? | default '/tmp')/iets-growth-cache"
  let workload = $"($env.PWD)/bench/workloads/nixos-desktop.nix"
  rm -rf $cache

  mut rows = []
  mut before = {trace_entries: 0, trace_bytes: 0, ir_entries: 0, ir_bytes: 0}
  for np in [$np1 $np2] {
    if not ($np | path exists) {
      print $"skipping ($np): not in the store"
      continue
    }
    with-env {IETS_CACHE_DIR: $cache} {
      ^$iets eval --impure --strict -I $"nixpkgs=($np)" $workload | ignore
    }
    let now = (sizes $cache)
    $rows = ($rows | append {
      nixpkgs: ($np | path basename | str substring 0..11)
      "trace entries": (count_delta $now.trace_entries $before.trace_entries)
      "trace size": (size_delta $now.trace_bytes $before.trace_bytes)
      "IR entries": (count_delta $now.ir_entries $before.ir_entries)
      "IR size": (size_delta $now.ir_bytes $before.ir_bytes)
    })
    $before = $now
  }
  rm -rf $cache
  $rows | to md --pretty
}
