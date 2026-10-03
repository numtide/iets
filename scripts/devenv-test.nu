#!/usr/bin/env nu
# `iets print-dev-env` against `nix print-dev-env --option eval-cache false`:
#  a. byte-identical bash for this repo's flake and a scratch mkShell flake;
#  b. --json identical modulo key order (jq -S);
#  c. warm run (cache hit) time, printed;
#  d. --gc-root creates the symlink and nix-store --gc --print-roots lists it;
#  e. a garbage-collected env output is rebuilt on the next (cached) run;
#  f. a shell whose package is absent from /nix/store gets it substituted;
#  g. `#attr` selects a shell;
#  h. a `git worktree` of this repo at a path nix has never copied: iets
#     prints the same bash without fetching the root tree;
#  i. `cache paths` lists the files the shell read, for a plain directory
#     and a git work tree, and nothing from the store; a path that is no
#     entry is an error;
#  j. it reads the cache entry, NUL-separated, and evaluates nothing;
#  k. a real direnv with nix/direnvrc reloads when such a file is edited.
# The scratch flakes sit on the nixpkgs this repo pins (lock written by hand,
# so nothing is fetched: both evaluators find the tree in the store by narHash).
#
# usage: scripts/devenv-test.nu            # uses target/release
#        IETS=target/debug/iets scripts/devenv-test.nu
use lib.nu [ROOT chomp diff-head iets-bin isolate require-real-nix run-in same-file show]

def --wrapped nixf [...args: string] { ^nix --extra-experimental-features "nix-command flakes" ...$args }

def ok [msg: string]: nothing -> bool { print $"ok   ($msg)"; true }
def bad [msg: string, ...lines: string]: nothing -> bool {
  print $"FAIL ($msg)"
  for l in $lines { print $"  ($l)" }
  false
}
def ms [t0: datetime, t1: datetime]: nothing -> int { (($t1 - $t0) / 1ms) | math floor }
def tail-of [file: string, n: int]: nothing -> string { open --raw $file | lines | last $n | str join "\n" }
def sorted-json [file: string]: nothing -> string { open --raw $file | ^jq -S . }

# nix-env WORK FLAKE FILE [ARGS]: nix print-dev-env from inside FLAKE (the script names <cwd>/outputs).
def --wrapped nix-env [work: string, flake: string, file: string, ...args: string]: nothing -> int {
  cd $flake
  let r = (nixf print-dev-env --option eval-cache "false" ...$args $flake | complete)
  $r.stdout | save -f $file
  $r.stderr | save -f $"($work)/nix.err"
  $r.exit_code
}
def --wrapped iets-env [ctx: record, flake: string, file: string, ...args: string]: nothing -> int {
  cd $flake
  let r = (^($ctx.iets) print-dev-env ...$args $flake | complete)
  $r.stdout | save -f $file
  $r.stderr | save -f $"($ctx.work)/iets.err"
  $r.exit_code
}

def run-tests [ctx: record]: nothing -> int {
  let work = $ctx.work
  let iets = $ctx.iets
  $env.IETS_CACHE_DIR = $"($work)/cache"
  mut res = []

  # The scratch flakes: `shell` for a-c, `scratch` for e-g (its default shell
  # names a package that must stay absent from the store until f).
  let locked = (open --raw flake.lock | from json | get nodes.nixpkgs.locked)
  let rev = $locked.rev
  let lock = {
    nodes: {
      nixpkgs: { locked: $locked, original: { owner: "NixOS", repo: "nixpkgs", rev: $rev, type: "github" } }
      root: { inputs: { nixpkgs: "nixpkgs" } }
    }
    root: "root"
    version: 7
  }
  let shell = $"($work)/shell"
  mkdir $shell
  r#'{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/@REV@";
  outputs = { self, nixpkgs }:
    let pkgs = nixpkgs.legacyPackages.x86_64-linux; in {
      devShells.x86_64-linux.default = pkgs.mkShell {
        packages = [ pkgs.hello pkgs.jq ];
        GREETING = "hi";
        shellHook = "export SCRATCH_FLAKE=\"$PWD\"\n";
      };
    };
}
'# | str replace -a "@REV@" $rev | save $"($shell)/flake.nix"
  $lock | to json | save $"($shell)/flake.lock"

  for flake in [$env.PWD $shell] {
    let name = ($flake | path basename)
    if (nix-env $work $flake $"($work)/nix.sh") != 0 { $res ++= [(bad $"nix print-dev-env ($name)" (tail-of $"($work)/nix.err" 3))] }
    if (iets-env $ctx $flake $"($work)/iets.sh" --no-tracing-cache) != 0 { $res ++= [(bad $"iets print-dev-env ($name)" (tail-of $"($work)/iets.err" 3))] }
    if (same-file $"($work)/nix.sh" $"($work)/iets.sh") {
      $res ++= [(ok $"a. bash identical: ($name) \((open --raw $"($work)/nix.sh" | into binary | bytes length) bytes\)")]
    } else {
      $res ++= [(bad $"a. bash differs: ($name)" (diff-head $"($work)/nix.sh" $"($work)/iets.sh" 8 | str join "\n"))]
    }

    nix-env $work $flake $"($work)/nix.json" --json
    iets-env $ctx $flake $"($work)/iets.json" --no-tracing-cache --json
    sorted-json $"($work)/nix.json" | save -f $"($work)/nix.sorted.json"
    sorted-json $"($work)/iets.json" | save -f $"($work)/iets.sorted.json"
    if (same-file $"($work)/nix.sorted.json" $"($work)/iets.sorted.json") {
      $res ++= [(ok $"b. json identical: ($name)")]
    } else {
      $res ++= [(bad $"b. json differs: ($name)" (diff-head $"($work)/nix.sorted.json" $"($work)/iets.sorted.json" 8 | str join "\n"))]
    }

    iets-env $ctx $flake $"($work)/miss.sh"          # cache miss, writes the entry
    let t0 = (date now)
    iets-env $ctx $flake $"($work)/hit.sh"
    let t1 = (date now)
    if (same-file $"($work)/nix.sh" $"($work)/hit.sh") {
      $res ++= [(ok $"c. warm run identical: ($name), (ms $t0 $t1) ms")]
    } else {
      $res ++= [(bad $"c. warm run differs: ($name)" (diff-head $"($work)/nix.sh" $"($work)/hit.sh" 5 | str join "\n"))]
    }
  }

  # ---- scratch flake -----------------------------------------------------------
  mut absent = ""
  mut absent_out = ""
  for p in [cowsay sl figlet lolcat] {
    let r = (nixf eval --raw --impure --expr $"\(builtins.getFlake \"($env.PWD)\"\).inputs.nixpkgs.legacyPackages.x86_64-linux.($p).outPath" | complete)
    if $r.exit_code != 0 { continue }
    let out = ($r.stdout | chomp)
    if not ($out | path exists) { $absent = $p; $absent_out = $out; break }
  }
  if $absent == "" {
    $absent = "cowsay"
    $absent_out = ""
    print "note: every candidate package is present; f. cannot show a substitution"
  }
  let scratch = $"($work)/scratch"
  mkdir $scratch
  r#'{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/@REV@";
  outputs = { self, nixpkgs }:
    let pkgs = nixpkgs.legacyPackages.x86_64-linux; in {
      devShells.x86_64-linux.default = pkgs.mkShell { packages = [ pkgs.@ABSENT@ ]; };
      devShells.x86_64-linux.other = pkgs.mkShell { FOO = "bar"; };
    };
}
'# | str replace -a "@REV@" $rev | str replace -a "@ABSENT@" $absent | save $"($scratch)/flake.nix"
  $lock | to json | save $"($scratch)/flake.lock"
  let root = $"($work)/roots/env"

  if (iets-env $ctx $scratch $"($work)/scratch.sh" --gc-root $root) != 0 { $res ++= [(bad "iets print-dev-env scratch" (tail-of $"($work)/iets.err" 5))] }
  let outpath = (^readlink $root | complete | get stdout | chomp)
  if $outpath =~ '^/nix/store/.*-nix-shell-env$' {
    $res ++= [(ok $"d. gc root symlink: ($root) -> ($outpath)")]
  } else {
    $res ++= [(bad $"d. gc root symlink missing or wrong: '($outpath)'")]
  }
  if (^nix-store --gc --print-roots | complete | get stdout | str contains $root) {
    $res ++= [(ok "d. nix-store --gc --print-roots lists the root")]
  } else {
    $res ++= [(bad "d. root not registered with the daemon")]
  }
  if $absent_out != "" {
    if ($absent_out | path exists) {
      $res ++= [(ok $"f. ($absent) was substituted: ($absent_out)")]
    } else {
      $res ++= [(bad $"f. ($absent_out) is still absent")]
    }
  }
  nix-env $work $scratch $"($work)/scratch-nix.sh"
  if (same-file $"($work)/scratch-nix.sh" $"($work)/scratch.sh") {
    $res ++= [(ok "a. bash identical: scratch")]
  } else {
    $res ++= [(bad "a. bash differs: scratch" (diff-head $"($work)/scratch-nix.sh" $"($work)/scratch.sh" 8 | str join "\n"))]
  }

  # e. simulate a GC of the env output: drop the root, delete the path, rerun on the warm cache.
  rm -f $root
  let deleted = ($outpath != "" and (^nix-store --delete $outpath | complete | get exit_code) == 0 and not ($outpath | path exists))
  if $deleted {
    let t0 = (date now)
    iets-env $ctx $scratch $"($work)/scratch2.sh" --gc-root $root --timings
    let t1 = (date now)
    let relinked = (^readlink $root | complete | get stdout | chomp) == $outpath
    if ($outpath | path exists) and $relinked and (same-file $"($work)/scratch.sh" $"($work)/scratch2.sh") {
      let notices = (open --raw $"($work)/iets.err" | lines | where ($it | str contains "cache: env output") | length)
      $res ++= [(ok $"e. deleted env output rebuilt on the next run, (ms $t0 $t1) ms \(($notices) fallback notice\)")]
    } else {
      $res ++= [(bad "e. env output not rebuilt" (tail-of $"($work)/iets.err" 5))]
    }
  } else {
    $res ++= [(bad $"e. could not delete ($outpath) \(another root holds it?\)")]
  }

  # g. #attr
  iets-env $ctx $scratch $"($work)/other.sh" ""
  do { cd $scratch; ^$iets print-dev-env $"($scratch)#other" | complete } | do {|r| $r.stdout | save -f $"($work)/other.sh"; $r.stderr | save -f $"($work)/iets.err" } $in
  do { cd $scratch; nixf print-dev-env --option eval-cache "false" $"($scratch)#other" | complete | get stdout } | save -f $"($work)/other-nix.sh"
  let has_foo = (open --raw $"($work)/other.sh" | lines | any {|l| $l == "FOO='bar'" })
  if $has_foo and (same-file $"($work)/other.sh" $"($work)/other-nix.sh") {
    $res ++= [(ok "g. #other selects devShells.x86_64-linux.other, identical to nix")]
  } else {
    $res ++= [(bad "g. #other" (diff-head $"($work)/other-nix.sh" $"($work)/other.sh" 5 | str join "\n") (tail-of $"($work)/iets.err" 3))]
  }
  let missing = (do { cd $scratch; ^$iets print-dev-env $"($scratch)#missing" | complete })
  $missing.stderr | save -f $"($work)/iets.err"
  if $missing.exit_code == 0 {
    $res ++= [(bad "g. #missing should fail")]
  } else if ($missing.stderr | str contains "does not provide attribute 'devShells.x86_64-linux.missing'") {
    $res ++= [(ok "g. #missing reports the attribute paths tried")]
  } else {
    $res ++= [(bad "g. #missing error text" (tail-of $"($work)/iets.err" 2))]
  }

  # h. iets first (cache off) on a worktree nix has never copied; then nix for the reference
  let fresh = $"($work)/fresh"
  ^git -C $env.PWD worktree add -q $fresh HEAD
  let r = (do { cd $fresh; with-env {IETS_CACHE: "0"} { ^$iets print-dev-env --no-cache $fresh | complete } })
  $r.stdout | save -f $"($work)/fresh.sh"
  $r.stderr | save -f $"($work)/iets.err"
  nix-env $work $fresh $"($work)/fresh-nix.sh"
  ^git -C $env.PWD worktree remove --force $fresh
  if $r.exit_code == 0 and not ($r.stderr | str contains "fetching") and (same-file $"($work)/fresh-nix.sh" $"($work)/fresh.sh") {
    $res ++= [(ok "h. worktree nix never saw: bash identical, nothing fetched")]
  } else {
    $res ++= [(bad $"h. worktree \(exit ($r.exit_code)\)" (tail-of $"($work)/iets.err" 3) (diff-head $"($work)/fresh-nix.sh" $"($work)/fresh.sh" 5 | str join "\n"))]
  }

  # i-k. the watch list. One flake, as a plain directory and as a git work tree.
  let watched = $"($work)/watched"
  mkdir $watched
  r#'{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/@REV@";
  outputs = { self, nixpkgs }: {
    devShells.x86_64-linux.default = import ./other.nix nixpkgs.legacyPackages.x86_64-linux;
  };
}
'# | str replace -a "@REV@" $rev | save $"($watched)/flake.nix"
  $lock | to json | save $"($watched)/flake.lock"
  r#'pkgs: pkgs.mkShell {
  GREETING = (builtins.fromJSON (builtins.readFile ./versions.json)).greeting;
  shellHook = "source ${./hook.sh}";
}
'# | save $"($watched)/other.nix"
  '{"greeting": "hi"}' | save $"($watched)/versions.json"
  "export HOOKED=1\n" | save $"($watched)/hook.sh"
  "nothing reads this\n" | save $"($watched)/unread.txt"
  let tracked = $"($work)/tracked"
  cp -r $watched $tracked
  ^git -C $tracked init -q
  ^git -C $tracked add -A
  ^git -C $tracked -c user.name=t -c user.email=t@example.org commit -q -m init

  let nope = (^$iets cache paths $"($work)/no-such-entry" | complete)
  if $nope.exit_code != 0 and ($nope.stdout | is-empty) {
    $res ++= [(ok "i. cache paths on a path that is no entry: an error, empty stdout")]
  } else {
    $res ++= [(bad $"i. cache paths on a non-entry \(exit ($nope.exit_code)\)" $nope.stdout $nope.stderr)]
  }
  for c in [[name dir extra]; [path $watched []] [git $tracked [".git/index"]]] {
    let entry = $"($work)/entry-($c.name)"
    iets-env $ctx $c.dir $"($work)/watched.sh" --entry-to $entry
    let p = (do { cd $c.dir; ^$iets cache paths (open --raw $entry | chomp) --json | complete })
    let paths = (try { $p.stdout | from json | get path } catch { [] })
    let want = ([flake.nix flake.lock other.nix versions.json hook.sh] ++ $c.extra | each {|f| $"($c.dir)/($f)" })
    let missing = ($want | where {|w| $w not-in $paths })
    let unwanted = (["/nix/store" $env.IETS_CACHE_DIR "unread.txt"] | where {|u| $paths | any {|p| $p | str contains $u } })
    if $p.exit_code == 0 and ($missing | is-empty) and ($unwanted | is-empty) {
      $res ++= [(ok $"i. ($c.name) flake: the files the shell read are watched, the store and unread.txt are not")]
    } else {
      $res ++= [(bad $"i. ($c.name) flake watch list" $"missing: ($missing)" $"unwanted: ($unwanted)" $p.stdout)]
    }
    # the NUL form is what the .envrc reads, and it holds the same paths
    let nul = (do { cd $c.dir; ^$iets cache paths (open --raw $entry | chomp) | complete })
    let n = ($nul.stdout | split row (char -i 0) | where $it != "" | length)
    if $nul.exit_code == 0 and $n == ($paths | length) and ($nul.stderr | is-empty) {
      $res ++= [(ok $"j. ($c.name) flake: cache paths reads the entry, NUL-separated, and evaluates nothing")]
    } else {
      $res ++= [(bad $"j. ($c.name) flake: cache paths" $"($n) NUL paths vs ($paths | length) json" $nul.stderr)]
    }
  }

  # k. a real direnv with nix/direnvrc and a cold cache: the first load watches versions.json, a
  # second prompt reloads nothing, an edit reloads. direnv compares mtimes in
  # whole seconds, so the edit is dated ahead.
  let dconf = $"($work)/direnv"
  mkdir $dconf
  cp nix/direnvrc $"($dconf)/direnvrc"
  "use flake\n" | save $"($tracked)/.envrc"
  let denv = {
    DIRENV_CONFIG: $dconf, XDG_DATA_HOME: $"($work)/direnv-data", DIRENV_LOG_FORMAT: "",
    IETS_CACHE_DIR: $"($work)/direnv-cache",
    PATH: ([($iets | path dirname)] ++ $env.PATH),
  }
  hide-env -i DIRENV_DIR DIRENV_FILE DIRENV_DIFF DIRENV_WATCHES
  let load = {|state: record| with-env ($denv | merge $state) { cd $tracked; ^direnv export json | complete | get stdout } }
  with-env $denv { cd $tracked; ^direnv allow }
  let first = (do $load {} | from json)
  let state = ($first | transpose k v | where k starts-with DIRENV_ | transpose -rd)
  let watches = (^direnv show_dump $first.DIRENV_WATCHES | from json | get path)
  let idle = (do $load $state)
  '{"greeting": "yo"}' | save -f $"($tracked)/versions.json"
  ^touch -d "5 seconds" $"($tracked)/versions.json"
  let again = (do $load $state)
  let reloaded = if ($again | is-empty) { "" } else { $again | from json | get -o GREETING | default "" }
  # `.git` itself is observed by kind only, and a git index refresh moves its
  # mtime, so watching it would reload the shell after every build
  let git_watched = ($"($tracked)/.git" in $watches)
  if $first.GREETING? == "hi" and $"($tracked)/versions.json" in $watches and ($idle | is-empty) and $reloaded == "yo" and not $git_watched {
    $res ++= [(ok $"k. direnv: ($watches | length) watches on the first load, .git not among them, idle without an edit, reloads on an edit of versions.json")]
  } else {
    $res ++= [(bad "k. direnv reload" $"first GREETING: ($first.GREETING?)" $"watches: ($watches)" $".git watched: ($git_watched)" $"idle: ($idle)" $"after the edit: ($reloaded)")]
  }

  let fail = ($res | where not $it | length)
  print $"($res | where $it | length) passed, ($fail) failed"
  if $fail == 0 { 0 } else { 1 }
}

def main [] {
  require-real-nix nix
  let iets = (iets-bin)
  cd $ROOT
  let ctx = {iets: $iets, work: (isolate devenv)}
  run-in $ctx.work { run-tests $ctx }
}
