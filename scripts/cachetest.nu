#!/usr/bin/env nu
# Semantics of the tracing evaluation cache (docs/explanation/caching.md): hits
# print identical output, every kind of change misses, uncacheable runs and
# failed runs write nothing, and each cache flag turns off the store it names.
use lib.nu [ROOT chomp iets-bin isolate run-in verdict]

# iets eval --strict --timings ARGS: rc, stdout, the `cache:` lines joined with
# spaces, and the `re-evaluating:` line the reporter prints on an invalidation.
# `--impure`: the workload observes the environment and the search path, which
# is what the cache has to invalidate on.
def --wrapped run-eval [iets: string, ...args: string]: nothing -> record {
  let r = (^$iets eval --impure --strict --timings ...$args | complete)
  {
    rc: $r.exit_code
    out: ($r.stdout | chomp)
    status: ($r.stderr | lines | where $it =~ '^cache:' | each {|l| $l + " " } | str join)
    why: ($r.stderr | lines | where $it =~ '^re-evaluating:' | str join " ")
  }
}

# iets search --timings over FILE with EXTRA flags: rc, stdout, the `cache:` lines.
def search-run [iets: string, file: string, dir: string, extra: list<string>]: nothing -> record {
  let r = (^$iets search -f $file --cache-dir $dir --timings ...$extra hello | complete)
  {
    rc: $r.exit_code
    out: ($r.stdout | chomp)
    status: ($r.stderr | lines | where $it =~ '^cache:' | str join " ")
  }
}

# name, mtime and size of every file under DIR
def fingerprint [dir: string]: nothing -> list {
  glob $"($dir)/**/*" | where ($it | path type) == "file" | sort | each {|f| ls -l $f | select name modified size | first }
}

def run-tests [iets: string, work: string]: nothing -> int {
  $env.IETS_CACHE_DIR = $"($work)/cache"
  $env.CACHETEST_VAR = "one"
  mkdir $"($work)/src/dir" $"($work)/inc1" $"($work)/inc2"
  '{ a = import ./lib.nix; b = builtins.attrNames (builtins.readDir ./dir); c = builtins.getEnv "CACHETEST_VAR"; d = import <inc/x.nix>; }' + "\n" | save $"($work)/src/main.nix"
  "40 + 2\n" | save $"($work)/src/lib.nix"
  "x\n" | save $"($work)/src/dir/one"
  "\"inc1\"\n" | save $"($work)/inc1/x.nix"
  "\"inc2\"\n" | save $"($work)/inc2/x.nix"
  let main = ["-I" $"inc=($work)/inc1" $"($work)/src/main.nix"]
  mut res = []

  let r = (run-eval $iets ...$main)
  let first = $r.out
  $res ++= [(verdict ($r.rc == 0 and $r.status =~ 'miss \(no entry\).*wrote') "first run misses and writes" $"first run misses and writes \(($r.status)\)")]
  # a cold run invalidated nothing, so it says nothing
  $res ++= [(verdict ($r.why == "") "a cold run reports no invalidation" $"cold run said ($r.why)")]
  let r = (run-eval $iets ...$main)
  $res ++= [(verdict ($r.rc == 0 and ($r.status | str contains "cache: hit") and $r.out == $first) "second run hits with identical output" $"second run hits with identical output \(($r.status)\)")]
  $res ++= [(verdict ($r.why == "") "a hit reports no invalidation" $"hit said ($r.why)")]

  "40 + 3\n" | save -f $"($work)/src/lib.nix"
  let r = (run-eval $iets ...$main)
  $res ++= [(verdict ($r.status =~ 'miss \(.*lib\.nix changed' and ($r.out | str contains "a = 43")) "editing an imported file misses" $"editing an imported file misses \(($r.status)\)")]
  $res ++= [(verdict ($r.why == $"re-evaluating: ($work)/src/lib.nix changed") "the invalidation names the changed file" $"changed file said ($r.why)")]
  # the message is the same whichever thread saw it first
  let whys = (1..5 | each {|_|
    "40 + 4\n" | save -f $"($work)/src/lib.nix"
    let a = (run-eval $iets ...$main)
    "40 + 3\n" | save -f $"($work)/src/lib.nix"
    let b = (run-eval $iets ...$main)
    [$a.why $b.why]
  } | flatten | uniq)
  $res ++= [(verdict ($whys == [$"re-evaluating: ($work)/src/lib.nix changed"]) "the same change always names the same file" $"repeated invalidations said ($whys)")]

  ^touch -d 2000-01-01 $"($work)/src/lib.nix"
  let r = (run-eval $iets ...$main)
  $res ++= [(verdict ($r.status =~ 'cache: hit.*1 rehashed') "touch on an imported file hits after one rehash" $"touch hits after one rehash \(($r.status)\)")]
  let r = (run-eval $iets ...$main)
  $res ++= [(verdict ($r.status =~ 'cache: hit.*0 rehashed') "refreshed stat fields skip the rehash next time" $"refresh persisted \(($r.status)\)")]

  # cp -p in place keeps the size, the mtime and the inode; only ctime says
  # the bytes are new
  let cd2 = $"($work)/ctime"
  mkdir $cd2
  "import ./lib.nix\n" | save -f $"($cd2)/main.nix"
  '"AAAA"' + "\n" | save -f $"($cd2)/lib.nix"
  '"BBBB"' + "\n" | save -f $"($cd2)/other.nix"
  ^touch -d "2020-01-01T00:00:00Z" $"($cd2)/lib.nix" $"($cd2)/other.nix"
  let cc = $"($work)/ctimecache"
  let c1 = (run-eval $iets --cache-dir $cc $"($cd2)/main.nix")
  ^cp -p $"($cd2)/other.nix" $"($cd2)/lib.nix"
  let c2 = (run-eval $iets --cache-dir $cc $"($cd2)/main.nix")
  $res ++= [(verdict ($c1.out == '"AAAA"' and $c2.out == '"BBBB"' and ($c2.status | str contains "miss ("))
    "a copy in place that keeps size, mtime and inode still misses" $"first=($c1.out) after cp -p=($c2.out) \(($c2.status)\)")]

  "y\n" | save $"($work)/src/dir/two"
  let r = (run-eval $iets ...$main)
  $res ++= [(verdict ($r.status =~ 'miss \(.*directory' and ($r.out | str contains '"two"')) "adding a file to a readDir'd directory misses" $"readDir change misses \(($r.status)\)")]
  $res ++= [(verdict ($r.why == $"re-evaluating: directory ($work)/src/dir has different entries") "the invalidation names the listed directory" $"readDir change said ($r.why)")]

  let r = (with-env {CACHETEST_VAR: "two"} { run-eval $iets ...$main })
  $res ++= [(verdict (($r.status | str contains 'miss (environment variable $CACHETEST_VAR changed)') and ($r.out | str contains 'c = "two"')) "changing a getEnv variable misses" $"getEnv change misses \(($r.status)\)")]
  $res ++= [(verdict ($r.why == 're-evaluating: environment variable $CACHETEST_VAR changed') "the invalidation names the environment variable" $"getEnv change said ($r.why)")]
  let r = (run-eval $iets ...$main)
  $res ++= [(verdict ($r.status =~ 'miss \(.*CACHETEST_VAR') "changing it back misses again (one entry per key)" $"one entry per key \(($r.status)\)")]

  let r = (run-eval $iets -I $"inc=($work)/inc2" $"($work)/src/main.nix")
  $res ++= [(verdict (($r.status | str contains "miss (no entry)") and ($r.out | str contains 'd = "inc2"')) "-I change misses (new key)" $"-I change misses \(($r.status)\)")]

  # the compiled-file cache folds `builtins.currentSystem` into an entry, and
  # `--pure-eval` removes that name, so the key holds the flag: an impure run
  # must not answer a pure one
  let pd = $"($work)/pure"
  mkdir $pd
  let pad = (1..30 | each {|i| $"# comment line ($i) padding padding padding padding" } | str join "\n")
  $pad + "\n" + '{ dflt = builtins.currentSystem or "fallback"; }' + "\n" | save -f $"($pd)/lib.nix"
  '{ outputs = _: { dflt = (import ./lib.nix).dflt; }; }' + "\n" | save -f $"($pd)/flake.nix"
  '{"nodes":{"root":{}},"root":"root","version":7}' + "\n" | save -f $"($pd)/flake.lock"
  let pc = $"($work)/purecache"
  let p1 = (^$iets eval --cache-dir $pc --pure-eval $"path:($pd)#dflt" | complete | get stdout | chomp)
  rm -rf $pc
  let p2 = (^$iets eval --cache-dir $pc --impure $"path:($pd)#dflt" | complete | get stdout | chomp)
  let p3 = (^$iets eval --cache-dir $pc --pure-eval $"path:($pd)#dflt" | complete | get stdout | chomp)
  $res ++= [(verdict ($p1 == '"fallback"' and $p2 != '"fallback"' and $p3 == '"fallback"')
    "a pure run never reads an entry an impure one compiled" $"pure=($p1) impure=($p2) pure again=($p3)")]

  let r = (run-eval $iets -E 'builtins.currentTime > 0')
  $res ++= [(verdict ($r.status | str contains "not written") "currentTime never caches" $"currentTime never caches \(($r.status)\)")]
  let r = (run-eval $iets -E 'builtins.currentTime > 0')
  $res ++= [(verdict ($r.status | str contains "miss (no entry)") "currentTime: still no entry on the second run" $"currentTime second run \(($r.status)\)")]

  let r = (run-eval $iets -E '(builtins.tryEval (builtins.fetchTarball "https://x/y.tar.gz")).success')
  $res ++= [(verdict ($r.rc != 0 and not ($r.status | str contains "wrote")) "a fetcher call fails and never caches" $"fetcher never caches \(($r.status)\)")]

  let before = (fingerprint $env.IETS_CACHE_DIR)
  let r = (run-eval $iets --no-tracing-cache ...$main)
  $res ++= [(verdict ($r.status == "") "--no-tracing-cache prints no cache line" $"--no-tracing-cache prints no cache line \(($r.status)\)")]
  run-eval $iets --no-tracing-cache -E 'import /nonexistent/nocache-probe.nix' | ignore
  let after = (fingerprint $env.IETS_CACHE_DIR)
  $res ++= [(verdict ($before == $after) "--no-tracing-cache never reads or writes the tracing store" "--no-tracing-cache changed the cache dir")]
  let r = (with-env {IETS_CACHE: "0"} { run-eval $iets ...$main })
  $res ++= [(verdict ($r.status == "") "IETS_CACHE=0 disables the cache" $"IETS_CACHE=0 \(($r.status)\)")]

  'throw "boom"' + "\n" | save -f $"($work)/src/lib.nix"
  let r = (run-eval $iets ...$main)
  $res ++= [(verdict ($r.rc != 0 and not ($r.status | str contains "wrote")) "a failed evaluation writes nothing" $"failed evaluation wrote \(($r.status)\)")]
  "1\n" | save -f $"($work)/src/lib.nix"
  let r = (run-eval $iets ...$main)
  $res ++= [(verdict ($r.rc == 0 and ($r.status | str contains "miss (")) "after the failure the fixed file misses, not a stale hit" $"after failure \(($r.status)\)")]

  # caching.md: errors are never cached. `nix eval` prints «error: …» for a
  # read that failed, with exit 0, and that read left no observation
  let ed = $"($work)/errs"
  mkdir $ed
  '{ b = import ./missing.nix; c = builtins.readDir ./missingdir; }' + "\n" | save -f $"($ed)/e.nix"
  let e1 = (^$iets nix eval --file $"($ed)/e.nix" | complete)
  '"B"' + "\n" | save -f $"($ed)/missing.nix"
  mkdir $"($ed)/missingdir"
  let e2 = (^$iets nix eval --file $"($ed)/e.nix" | complete)
  $res ++= [(verdict ($e1.exit_code == 0 and ($e1.stdout | str contains "«error:") and $e2.exit_code == 0 and not ($e2.stdout | str contains "«error:"))
    "a result that holds an error placeholder is never cached" $"first=($e1.stdout | str substring 0..60) second=($e2.stdout | str substring 0..60)")]

  # a lint warning is an effect: the entry records it and a hit prints it again
  "http://z.example\n" | save -f $"($work)/src/url.nix"
  let w1 = (run-eval $iets --lint url-literal=warn $"($work)/src/url.nix")
  let w2 = (run-eval $iets --lint url-literal=warn $"($work)/src/url.nix")
  let r1 = (^$iets eval --impure --strict --timings --lint url-literal=warn $"($work)/src/url.nix" | complete)
  let warned = ($r1.stderr | lines | where ($it | str starts-with "warning: [url-literal]") | length)
  $res ++= [(verdict ($w1.status =~ 'miss' and ($w2.status | str contains "cache: hit") and $warned == 1 and ($r1.stderr | str contains "cache: hit")) "a lint warning is replayed on a hit" $"lint warning replay \(($w2.status) warned=($warned)\)")]

  # the file on the command line is in the read set, not in the key: editing it
  # is an ordinary invalidation that names it, and the command keeps one entry
  let kd = $"($work)/keycache"
  let top = $"($work)/key/top.nix"
  mkdir $"($work)/key"
  "import ./dep.nix\n" | save -f $top
  "41 + 1\n" | save -f $"($work)/key/dep.nix"
  let k = (run-eval $iets --cache-dir $kd $top)
  $res ++= [(verdict (($k.status | str contains "miss (no entry)") and $k.why == "") "a file nothing ran before is a silent cold run" $"new file argument \(($k.status) ($k.why)\)")]
  let k = (run-eval $iets --cache-dir $kd $top)
  $res ++= [(verdict ($k.status | str contains "cache: hit") "the file argument hits" $"file argument hit \(($k.status)\)")]
  "import ./dep.nix # edited\n" | save -f $top
  let k = (run-eval $iets --cache-dir $kd $top)
  $res ++= [(verdict ($k.why == $"re-evaluating: ($top) changed") "editing the file argument names it" $"edited file argument said ($k.why)")]
  let k = (run-eval $iets --cache-dir $kd $top)
  $res ++= [(verdict ($k.status | str contains "cache: hit") "and the edited file hits next time" $"edited file argument \(($k.status)\)")]
  let entries = (ls $"($kd)/trace" | where type == dir | length)
  $res ++= [(verdict ($entries == 1) "one command keeps one entry across edits" $"($entries) entries after editing the file argument")]
  rm $top
  let k = (run-eval $iets --cache-dir $kd $top)
  $res ++= [(verdict ($k.rc != 0 and $k.why == $"re-evaluating: ($top) is gone") "a removed file argument says it is gone" $"removed file argument said ($k.why)")]

  # the same for a flake: its directory is the key, and flake.nix and
  # flake.lock are read like any other file
  let fd = $"($work)/flakecache"
  let fl = $"($work)/fl"
  mkdir $fl
  '{ outputs = _: { x = 41 + 1; }; }' + "\n" | save -f $"($fl)/flake.nix"
  '{"nodes":{"root":{}},"root":"root","version":7}' + "\n" | save -f $"($fl)/flake.lock"
  let f = (run-eval $iets --cache-dir $fd $"path:($fl)#x")
  $res ++= [(verdict ($f.rc == 0 and ($f.status | str contains "miss (no entry)") and $f.why == "") "a flake nothing ran before is a silent cold run" $"new flake \(($f.status) ($f.why)\)")]
  let f = (run-eval $iets --cache-dir $fd $"path:($fl)#x")
  $res ++= [(verdict ($f.status | str contains "cache: hit") "the flake hits" $"flake hit \(($f.status)\)")]
  '{ outputs = _: { x = 40 + 2; }; }' + "\n" | save -f $"($fl)/flake.nix"
  let f = (run-eval $iets --cache-dir $fd $"path:($fl)#x")
  $res ++= [(verdict ($f.why == $"re-evaluating: source tree ($fl) changed" and $f.out == "42") "editing flake.nix names the flake's tree" $"edited flake.nix said ($f.why)")]
  '{"nodes":{"root":{}},"root":"root","version":7} ' + "\n" | save -f $"($fl)/flake.lock"
  let f = (run-eval $iets --cache-dir $fd $"path:($fl)#x")
  $res ++= [(verdict ($f.why == $"re-evaluating: ($fl)/flake.lock changed") "editing flake.lock names it" $"edited flake.lock said ($f.why)")]
  let entries = (ls $"($fd)/trace" | where type == dir | length)
  $res ++= [(verdict ($entries == 1) "one flake command keeps one entry across edits" $"($entries) entries after editing the flake")]

  # a `path:` flake has the newest mtime of its tree as `lastModified`, so the
  # same bytes with another date are another answer, where any other tree
  # with the same bytes is a hit
  let lm = $"($work)/lm"
  mkdir $lm
  "{ outputs = { self }: { x = self.lastModified; }; }\n" | save -f $"($lm)/flake.nix"
  "{\"nodes\":{\"root\":{}},\"root\":\"root\",\"version\":7}\n" | save -f $"($lm)/flake.lock"
  ^touch -d "2020-01-01T00:00:00Z" $"($lm)/flake.nix" $"($lm)/flake.lock" $lm
  let f = (run-eval $iets --cache-dir $fd $"path:($lm)#x")
  ^touch -d "2021-01-01T00:00:00Z" $"($lm)/flake.nix"
  let g = (run-eval $iets --cache-dir $fd $"path:($lm)#x")
  $res ++= [(verdict ($f.out == "1577836800" and $g.out == "1609459200" and $g.why == $"re-evaluating: source tree ($lm) has another modification time")
    "a path: flake with the same bytes and a newer date evaluates again" $"($f.out) then ($g.out), said ($g.why)")]

  # --cache-dir points at a directory the user names: iets owns only the
  # store subdirectories it makes there, and never removes anything else
  mkdir $"($work)/mine"
  "PRECIOUS\n" | save -f $"($work)/mine/unrelated.txt"
  let r = (run-eval $iets --cache-dir $"($work)/mine" -E '2')
  $res ++= [(verdict ($r.rc == 0 and (open $"($work)/mine/unrelated.txt" | str contains "PRECIOUS")) "--cache-dir keeps a file it did not create" "--cache-dir deleted an unrelated file")]
  $res ++= [(verdict (($"($work)/mine/trace" | path exists) and ($"($work)/mine/ir" | path exists)) "--cache-dir holds the stores in subdirectories" "--cache-dir made no store subdirectory")]

  # every cache flag turns off the store it names, and no other: a store that
  # runs makes its subdirectory, a store that is off makes nothing
  let cases = [
    { flags: [], trace: true, ir: true, what: "no cache flag" }
    { flags: ["--no-tracing-cache"], trace: false, ir: true, what: "--no-tracing-cache" }
    { flags: ["--no-ir-cache"], trace: true, ir: false, what: "--no-ir-cache" }
    { flags: ["--no-cache"], trace: false, ir: false, what: "--no-cache" }
  ]
  for c in ($cases | enumerate) {
    let dir = $"($work)/stores($c.index)"
    let r = (run-eval $iets --cache-dir $dir ...$c.item.flags ...$main)
    let got = { trace: ($"($dir)/trace" | path exists), ir: ($"($dir)/ir" | path exists) }
    let ok = ($r.rc == 0 and $got.trace == $c.item.trace and $got.ir == $c.item.ir)
    $res ++= [(verdict $ok $"($c.item.what): tracing=($c.item.trace) ir=($c.item.ir)" $"($c.item.what): wanted tracing=($c.item.trace) ir=($c.item.ir), got tracing=($got.trace) ir=($got.ir)")]
  }

  # a directory that is not iets's is refused, not wiped
  mkdir $"($work)/theirs/trace"
  "PRECIOUS\n" | save -f $"($work)/theirs/trace/notes.txt"
  let r = (^$iets eval --strict --timings --cache-dir $"($work)/theirs" -E '2' | complete)
  $res ++= [(verdict ($r.exit_code == 0 and ($r.stderr | str contains "is not an iets cache") and (open $"($work)/theirs/trace/notes.txt" | str contains "PRECIOUS")) "a foreign store directory is refused, not wiped" $"foreign directory \(($r.stderr | lines | last)\)")]

  # --ttl in days must fit in seconds
  let g = (^$iets cache gc --cache-dir $"($work)/ttl" --ttl 213503982334602 | complete)
  $res ++= [(verdict ($g.exit_code == 2 and ($g.stderr | str contains "--ttl wants a number of days")) "a --ttl whose seconds overflow is refused" $"cache gc --ttl overflow \(rc=($g.exit_code): ($g.stderr | chomp)\)")]

  # identical output on a hit for a lang test with paths and positions
  let t = "tests/lang/eval-okay-getattrpos.nix"
  let o1 = (^$iets eval --strict $t | complete | get stdout | chomp)
  let r2 = (^$iets eval --strict --timings $t | complete)
  let hits = ($r2.stderr | lines | where ($it | str contains "cache: hit") | length)
  $res ++= [(verdict ($o1 == ($r2.stdout | chomp) and $hits == 1) "lang test hit prints identical output" "lang test hit")]

  # search takes the same cache and worker flags as eval (docs/reference/commands.md)
  let pkgs = $"($work)/src/pkgs.nix"
  '{ }: { hello = derivation { name = "hello-1"; system = builtins.currentSystem; builder = "/bin/sh"; args = [ ]; } // { meta.description = "greets"; }; }' + "\n" | save -f $pkgs
  let sdir = $"($work)/searchcache"
  let s1 = (search-run $iets $pkgs $sdir [])
  let s2 = (search-run $iets $pkgs $sdir [])
  $res ++= [(verdict ($s1.rc == 0 and $s2.rc == 0 and ($s1.status =~ 'miss') and ($s2.status | str contains "cache: hit") and $s1.out == $s2.out) "search --cache-dir: the index misses then hits there" $"search cache-dir \(($s1.status) / ($s2.status)\)")]
  let before = (fingerprint $sdir)
  let s3 = (search-run $iets $pkgs $sdir [--no-tracing-cache])
  $res ++= [(verdict ($s3.rc == 0 and $s3.status == "" and (fingerprint $sdir) == $before and $s3.out == $s1.out) "search --no-tracing-cache: no cache line, nothing read or written" $"search --no-tracing-cache \(($s3.status)\)")]
  let idir = $"($work)/searchcache-noir"
  let s4 = (search-run $iets $pkgs $idir [--no-ir-cache])
  $res ++= [(verdict ($s4.rc == 0 and not ($"($idir)/ir" | path exists)) "search --no-ir-cache: no compiled-file cache" $"search --no-ir-cache, ir dir exists=($"($idir)/ir" | path exists)")]
  let s5 = (search-run $iets $pkgs $sdir [--workers 2])
  $res ++= [(verdict ($s5.rc == 0 and $s5.out == $s1.out) "search --workers N: the same results" $"search --workers \(rc=($s5.rc)\)")]
  let s6 = (^$iets search -f $pkgs --workers 0 hello | complete)
  $res ++= [(verdict ($s6.exit_code == 2 and ($s6.stderr | str contains "--workers wants a number >= 1")) "search --workers 0 is a usage error" $"search --workers 0 \(rc=($s6.exit_code): ($s6.stderr)\)")]

  let fail = if ($res | all {|r| $r }) { 0 } else { 1 }
  print $"pass=($res | where $it | length) fail=($fail)"
  $fail
}

def main [] {
  let iets = (iets-bin)
  cd $ROOT
  let work = (isolate cachetest)
  run-in $work { run-tests $iets $work }
}
