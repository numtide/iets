#!/usr/bin/env nu
# Usage: bench/chart.nu [--pins FILE] [--out PREFIX] RESULT.json...
# Draws the hyperfine JSON from bench/run.nu as PREFIX-light.svg and
# PREFIX-dark.svg: one row per workload, warm and -cold side by side.

# Colours are keyed by series name, in an order validated with the dataviz
# palette checker for adjacent bars on GitHub's light and dark pages, in
# the order nix, lix, fix, iets. Reordering the bars needs a new check.
const THEMES = {
  light: { ink: "#0b0b0b", ink2: "#52514e", muted: "#6b6a65", axis: "#c3c2b7", series: { "nix": "#2a78d6", "lix": "#008300", "fix w1": "#e87ba4", "fix w4": "#eda100", "iets w1": "#1baf7a", "iets w4": "#4a3aa7" } }
  dark: { ink: "#f0f6fc", ink2: "#c3c2b7", muted: "#9198a1", axis: "#3d444d", series: { "nix": "#3987e5", "lix": "#008300", "fix w1": "#d55181", "fix w4": "#c98500", "iets w1": "#199e70", "iets w4": "#9085e9" } }
}

const LABELS = { "nix": "Nix", "lix": "Lix", "fix w1": "fix, 1 worker", "fix w4": "fix, 4 workers", "iets w1": "iets, 1 worker", "iets w4": "iets, 4 workers" }

def label [n: string]: nothing -> string { $LABELS | get -o $n | default $n }

def esc []: string -> string { str replace -a "&" "&amp;" | str replace -a "<" "&lt;" | str replace -a ">" "&gt;" }

def fmt-s [s: float]: nothing -> string {
  $"($s | into string --decimals 2) s"
}

def load [files: list<string>]: nothing -> table {
  $files | each {|f|
    let stem = ($f | path parse | get stem)
    let cold = ($stem | str ends-with "-cold")
    let w = ($stem | str replace -r '-cold$' "")
    open $f | get results | each {|r|
      { workload: $w, cache: (if $cold { "cold" } else { "warm" }), name: $r.command, mean: $r.mean, min: $r.min, max: $r.max, runs: ($r.times | length) }
    }
  } | flatten
}

def svg [rows: table, names: list<string>, caption: list<string>, t: record]: nothing -> string {
  let W = 800
  let labw = 100
  let gap = 36
  let pw = (($W - 2 * $labw - $gap - 8) / 2 | math floor)
  let bh = 14
  let bgap = 8
  let ph = ($names | length) * ($bh + $bgap)
  let rowh = $ph + 46
  let workloads = ($rows | get workload | uniq)
  let top = 58
  let H = $top + ($workloads | length) * $rowh + ($caption | length) * 16 + 8
  mut out = [
    $'<svg xmlns="http://www.w3.org/2000/svg" width="($W)" height="($H)" viewBox="0 0 ($W) ($H)" font-family="system-ui,-apple-system,Segoe UI,sans-serif" font-size="12">'
    $'<text x="0" y="16" font-size="15" font-weight="600" fill="($t.ink)">Eval wall time, lower is better</text>'
  ]
  mut lx = 0
  for i in 0..<($names | length) {
    let n = ($names | get $i)
    $out = ($out | append $'<rect x="($lx)" y="30" width="10" height="10" rx="2" fill="($t.series | get $n)"/><text x="($lx + 14)" y="39" fill="($t.ink2)">(label $n | esc)</text>')
    $lx = $lx + 14 + (label $n | str length) * 7 + 18
  }
  mut y = $top
  for w in $workloads {
    let wr = ($rows | where workload == $w)
    let xmax = ($wr | get max | math max)
    let base = ($wr | where name == ($names | first))
    for c in [warm cold] {
      let cr = ($wr | where cache == $c)
      if ($cr | is-empty) { continue }
      let px = if $c == "warm" { $labw } else { $labw * 2 + $pw + $gap }
      let nruns = ($cr | get runs | math min)
      $out = ($out | append $'<text x="($px - $labw)" y="($y + 14)" font-weight="600" fill="($t.ink)">($w | esc)</text><text x="($px + $pw)" y="($y + 14)" text-anchor="end" fill="($t.muted)">($c) page cache, ($nruns) runs</text>')
      let ref = ($base | where cache == $c | get -o 0.mean | default 0.0)
      let y0 = $y + 24
      $out = ($out | append $'<line x1="($px)" x2="($px)" y1="($y0 - 4)" y2="($y0 + $ph)" stroke="($t.axis)"/>')
      for i in 0..<($names | length) {
        let r = ($cr | where name == ($names | get $i) | get -o 0)
        if $r == null { continue }
        let by = $y0 + $i * ($bh + $bgap)
        let s = {|v| $px + ($v / $xmax * ($pw - 150)) | math round -p 1 }
        let bw = ([((do $s $r.mean) - $px) 4.0] | math max)
        let mid = $by + $bh / 2
        let rel = if $i == 0 or $ref == 0.0 { "" } else {
          let k = $ref / $r.mean
          if ($k - 1 | math abs) < 0.05 { "· ≈ Nix" } else if $k >= 1 { $"· ($k | math round -p 1)× faster" } else { $"· ($r.mean / $ref | math round -p 1)× slower" }
        }
        $out = ($out | append [
          $'<text x="($px - 6)" y="($mid + 4)" text-anchor="end" fill="($t.ink2)">(label $r.name | esc)</text>'
          $'<g><title>($r.name | esc), ($w) ($c): mean (fmt-s $r.mean), min (fmt-s $r.min), max (fmt-s $r.max), ($r.runs) runs</title>'
          $'<path d="M($px) ($by)h($bw - 4)a4 4 0 0 1 4 4v($bh - 8)a4 4 0 0 1 -4 4h-($bw - 4)z" fill="($t.series | get $r.name)"/>'
          $'<line x1="(do $s $r.min)" x2="(do $s $r.max)" y1="($mid)" y2="($mid)" stroke="($t.ink)" stroke-width="1.5"/>'
          $'<line x1="(do $s $r.min)" x2="(do $s $r.min)" y1="($mid - 4)" y2="($mid + 4)" stroke="($t.ink)" stroke-width="1.5"/>'
          $'<line x1="(do $s $r.max)" x2="(do $s $r.max)" y1="($mid - 4)" y2="($mid + 4)" stroke="($t.ink)" stroke-width="1.5"/></g>'
          $'<text x="((do $s $r.max) + 6)" y="($mid + 4)" fill="($t.ink)">(fmt-s $r.mean)<tspan dx="6" fill="($t.ink2)">($rel)</tspan></text>'
        ])
      }
    }
    $y = $y + $rowh
  }
  for l in $caption {
    $out = ($out | append $'<text x="0" y="($y + 4)" font-size="11" fill="($t.muted)">($l | esc)</text>')
    $y = $y + 16
  }
  $out | append "</svg>" | str join "\n"
}

def main [--pins: string, --out: string = "bench/results/eval", ...files: string] {
  let rows = (load $files)
  let names = ($rows | get name | uniq)
  let unknown = ($names | where $it not-in ($THEMES.light.series | columns))
  if ($unknown | is-not-empty) { error make { msg: $"no validated colour for ($unknown | str join ', '); add one to THEMES" } }
  let pinned = if $pins == null { [] } else { open --raw $pins | lines | where $it =~ '^(host|nix|lix|fix|iets|nixpkgs|hyperfine|date):' and $it !~ ': not run$' }
  let caption = ($pinned | append "Bars: mean. Whiskers: min to max. Eval caches off; cold drops the page cache before each run.")
  for th in [light dark] {
    svg $rows $names $caption ($THEMES | get $th) | save -f $"($out)-($th).svg"
  }
}
