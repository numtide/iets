#!/usr/bin/env nu
# One fixture per lint in tests/lint/<name>.nix (docs/reference/lints.md). Each runs
# three times, from tests/ so paths print as /pwd/lint/...:
#   default levels: no `warning: [` line at all (parity);
#   --lint NAME=warn: stderr equals <name>.warn.exp;
#   --lint NAME=error: exit 1 and the first stderr line is `error: [NAME] ...`.
# Then the framework cases in tests/lint/cases/: IETS_LINT, iets.toml,
# `all=` and `family:` selectors, a name beating a family or `all` inside
# one source (a table, the flags, IETS_LINT), one warning per site, tryEval not
# catching a lint error, an unknown lint name (a warning in iets.toml and
# IETS_LINT, a usage error in `--lint`), and a warning replayed on a
# tracing-cache hit, and a finding as a record with its byte span and its edit.
# Then the scope cases in tests/lint/scope/: a site outside the entry's
# directory is silent unless `--lint-scope all`, `--lint-root`, or the
# `scope`/`roots` keys of iets.toml open it; `fod` names the user's
# derivation, not the wrapper's binding, while `drv-list` is silent through
# a wrapper outside the root; and a scratch flake with a `path:` input
# (built in a temporary directory, the lock written with the narHash iets
# computes) whose input is silent until `--lint-scope all` or
# `[lint.inputs.dep]`, and inside which `"${./file}"` is not a re-copy and
# `"${./dir}"` is `dir-copy-flake`. Then the `# iets: allow` cases in
# tests/lint/allow/: an own-line or trailing comment silences the named
# lints, a family, or `all` on the line it covers, at `warn` and at `error`,
# for a compile-time and a runtime site, through the IR cache and the
# tracing cache; the next line only; an unknown directive is `unknown-allow`.
# UPDATE=1 rewrites the .warn.exp files.
use lib.nu [chomp flags-of glob-sorted verdict]

def norm [pwd: string]: string -> string { $in | str replace -a $pwd "/pwd" | chomp }

def main [] {
  cd ($env.FILE_PWD | path join "..")
  let iets = ($env.IETS? | default ($env.PWD | path join "target/release/iets"))
  $env.HOME = "/fake-home"
  cd tests
  let pwd = $env.PWD
  # git refuses to track a `.git` entry, so the fixture gets one at run time
  mkdir lint/git-in-source.d/.git
  mut res = []
  for f in (glob-sorted lint/*.nix) {
    let n = ($f | str replace -r '\.nix$' '')
    let name = ($n | path basename)
    let flags = (flags-of $n)
    let d = (^$iets eval --strict --no-tracing-cache ...$flags $f | complete)
    let quiet = (($d.stderr | lines | where ($it | str starts-with "warning: [") | where not ($it | str starts-with $"warning: [($name)]") | length) == 0)
    let w = (^$iets eval --strict --no-tracing-cache ...$flags --lint $"($name)=warn" $f | complete)
    let got = ($w.stderr | norm $pwd)
    if ($env.UPDATE? | default "") == "1" { $got + "\n" | save -f $"($n).warn.exp" }
    let exp = (if ($"($n).warn.exp" | path exists) { open --raw $"($n).warn.exp" | chomp } else { "" })
    let e = (^$iets eval --strict --no-tracing-cache ...$flags --lint $"($name)=error" $f | complete)
    let first = ($e.stderr | lines | get -o 0 | default "")
    let fires = ($got | str contains $"warning: [($name)]")
    let ok = $quiet and $fires and ($got == $exp) and ($e.exit_code == 1) and ($first | str starts-with $"error: [($name)]")
    let why = (if not $quiet { "fires at the default level" } else if not $fires { "does not fire at warn" } else if $got != $exp { "stderr differs from .warn.exp" } else { $"error run: rc=($e.exit_code) first line '($first)'" })
    $res ++= [(verdict $ok $name $"($name): ($why)")]
  }
  rm -rf lint/git-in-source.d/.git

  let url_warn = "warning: [url-literal] URL literals are discouraged. Consider using a string literal \"http://example.com\" instead"
  let r = (with-env {IETS_LINT: "url-literal=warn"} { ^$iets eval --no-tracing-cache lint/url-literal.nix | complete })
  $res ++= [(verdict ($r.stderr | str contains $url_warn) "IETS_LINT sets a level" "IETS_LINT ignored")]
  let r = (with-env {IETS_LINT: "url-literal=warn"} { ^$iets eval --no-tracing-cache --lint url-literal=off lint/url-literal.nix | complete })
  $res ++= [(verdict (not ($r.stderr | str contains "warning:")) "--lint wins over IETS_LINT" "--lint did not override IETS_LINT")]
  let r = (^$iets eval --no-tracing-cache lint/cases/toml/main.nix | complete)
  $res ++= [(verdict ($r.stderr | str contains $url_warn) "iets.toml sets a level" "iets.toml ignored")]
  let r = (^$iets eval --no-tracing-cache lint/cases/untrusted/main.nix | complete)
  $res ++= [(verdict ($r.exit_code != 0 and ($r.stderr | str contains "'nix-bin-dir' may not be set in a project file")) "iets.toml: a project file may not set nix-bin-dir" $"untrusted: rc=($r.exit_code) ($r.stderr)")]
  let r = (with-env {XDG_CONFIG_HOME: $"($pwd)/lint/user"} { ^$iets eval --no-tracing-cache lint/url-literal.nix | complete })
  $res ++= [(verdict ($r.exit_code == 1 and ($r.stderr | str contains "error: [url-literal]")) "the user iets.toml sets a level" $"user toml: rc=($r.exit_code) ($r.stderr)")]
  let r = (with-env {XDG_CONFIG_HOME: $"($pwd)/lint/user"} { ^$iets eval --no-tracing-cache lint/cases/toml/main.nix | complete })
  $res ++= [(verdict ($r.exit_code == 0 and ($r.stderr | str contains $url_warn)) "the project iets.toml wins over the user's" $"user below project: rc=($r.exit_code) ($r.stderr)")]
  let r = (^$iets eval --no-tracing-cache --lint family:legacy=warn lint/url-literal.nix | complete)
  $res ++= [(verdict ($r.stderr | str contains $url_warn) "family:legacy selects the family" "family selector ignored")]
  let r = (^$iets eval --no-tracing-cache --lint all=warn --lint url-literal=off lint/url-literal.nix | complete)
  $res ++= [(verdict (not ($r.stderr | str contains "[url-literal]")) "later --lint wins over all=" "all= not overridden")]
  let r = (^$iets eval --no-tracing-cache --lint url-literal=off --lint all=warn lint/url-literal.nix | complete)
  $res ++= [(verdict (not ($r.stderr | str contains "[url-literal]")) "a --lint name beats a later all=" "a name given first lost to a later all=")]
  let r = (with-env {IETS_LINT: "url-literal=off,family:legacy=warn"} { ^$iets eval --no-tracing-cache lint/url-literal.nix | complete })
  $res ++= [(verdict (not ($r.stderr | str contains "[url-literal]")) "an IETS_LINT name beats a later family" "an IETS_LINT name lost to a later family")]
  let r = (^$iets eval --no-tracing-cache lint/cases/family/main.nix | complete)
  $res ++= [(verdict ($r.exit_code == 0 and ($r.stderr | str contains "warning: [absolute-path-literal]")) "iets.toml: a lint name beats its family" $"toml family: rc=($r.exit_code) ($r.stderr)")]
  let r = (^$iets eval --no-tracing-cache lint/cases/family/member.nix | complete)
  $res ++= [(verdict ($r.exit_code == 1 and ($r.stderr | str contains "error: [short-path-literal]")) "iets.toml: the rest of the family keeps its level" $"toml family member: rc=($r.exit_code) ($r.stderr)")]
  let r = (^$iets eval --strict --no-tracing-cache --lint eq-type-mismatch=warn lint/cases/dedup.nix | complete)
  let n = ($r.stderr | lines | where ($it | str starts-with "warning: [eq-type-mismatch]") | length)
  $res ++= [(verdict ($n == 1) "one warning per site" $"one warning per site: got ($n)")]
  let r = (^$iets eval --no-tracing-cache --lint eq-type-mismatch=error lint/cases/tryeval.nix | complete)
  $res ++= [(verdict ($r.exit_code == 1 and ($r.stderr | str contains "error: [eq-type-mismatch]")) "tryEval does not catch a lint error" $"tryEval caught the lint error \(rc=($r.exit_code)\)")]
  let r = (^$iets lint list --lint url-literal=error | complete)
  $res ++= [(verdict (($r.stdout | lines | where ($it | str starts-with "url-literal ") | first) =~ 'error') "lint list shows effective levels" "lint list")]
  let r = (^$iets eval --no-tracing-cache --lint nope=warn lint/url-literal.nix | complete)
  $res ++= [(verdict ($r.exit_code == 2 and ($r.stderr | str contains "unknown lint 'nope'")) "--lint: an unknown lint name is a usage error" "unknown lint name")]
  let r = (^$iets eval --no-tracing-cache lint/cases/unknown/main.nix | complete)
  let ok = ($r.exit_code == 0 and ($r.stderr | str contains "iets.toml: iets does not know the lint 'with' and ignores it") and ($r.stderr | str contains "does not know the lint 'family:scope'") and ($r.stderr | str contains $url_warn))
  $res ++= [(verdict $ok "iets.toml: an unknown lint name is a warning and the rest counts" $"unknown name in iets.toml: rc=($r.exit_code) ($r.stderr)")]
  let r = (with-env {IETS_LINT: "nope=warn,url-literal=warn"} { ^$iets eval --no-tracing-cache --log-format internal-json lint/url-literal.nix | complete })
  let ok = ($r.exit_code == 0 and ($r.stderr | str contains '"code":"unknown-lint"') and ($r.stderr | str contains "IETS_LINT: iets does not know the lint 'nope'") and ($r.stderr | str contains "[url-literal]"))
  $res ++= [(verdict $ok "IETS_LINT: an unknown lint name is an unknown-lint record" $"unknown name in IETS_LINT: rc=($r.exit_code) ($r.stderr)")]

  let cache = (mktemp -d --tmpdir iets-linttest.XXXXXX)
  let r1 = (with-env {IETS_CACHE_DIR: $cache} { ^$iets eval --strict --timings --lint url-literal=warn lint/url-literal.nix | complete })
  let r2 = (with-env {IETS_CACHE_DIR: $cache} { ^$iets eval --strict --timings --lint url-literal=warn lint/url-literal.nix | complete })
  rm -rf $cache
  let hit = ($r2.stderr | str contains "cache: hit")
  let same = (($r1.stderr | lines | where ($it | str starts-with "warning:")) == ($r2.stderr | lines | where ($it | str starts-with "warning:")))
  $res ++= [(verdict ($hit and $same and ($r1.stderr | str contains $url_warn)) "a warning is replayed on a cache hit" $"cache replay: hit=($hit) same=($same)")]

  # a finding as a record: the byte span of the site and the edit, also from a
  # tracing-cache hit, and at `error`
  let site = '"end":18,'
  let edit = '"replacement":"\"http://example.com\"","sid":'
  let cache = (mktemp -d --tmpdir iets-linttest.XXXXXX)
  let json = [--log-format internal-json --lint url-literal=warn lint/url-literal.nix]
  let r1 = (with-env {IETS_CACHE_DIR: $cache} { ^$iets eval --strict --timings ...$json | complete })
  let r2 = (with-env {IETS_CACHE_DIR: $cache} { ^$iets eval --strict --timings ...$json | complete })
  rm -rf $cache
  let has = {|r| ($r.stderr | str contains $site) and ($r.stderr | str contains $edit) and ($r.stderr | str contains '"start":0}') }
  $res ++= [(verdict ((do $has $r1) and (do $has $r2) and ($r2.stderr | str contains "cache: hit")) "a finding record has the span and the edit, on a cache hit too" $"finding record: ($r1.stderr) ($r2.stderr)")]
  let r = (^$iets eval --no-tracing-cache --log-format internal-json --lint eq-type-mismatch=error lint/cases/tryeval.nix | complete)
  $res ++= [(verdict ($r.exit_code == 1 and ($r.stderr | str contains '"end":27,') and ($r.stderr | str contains '"start":19}') and not ($r.stderr | str contains "replacement")) "a finding at error has the span and no edit" $"finding at error: ($r.stderr)")]

  # scope: lint/scope/lib is outside the entry directories lint/scope/{root,all,roots}
  let outside = "at /pwd/lint/scope/lib/url.nix:1:1"
  let r = (^$iets eval --no-tracing-cache --lint url-literal=warn lint/scope/root/main.nix | complete)
  $res ++= [(verdict (not ($r.stderr | str contains "warning:")) "scope: a site outside the root is silent" "scope: outside site reported")]
  let r = (^$iets eval --no-tracing-cache --lint url-literal=warn --lint-scope all lint/scope/root/main.nix | complete)
  $res ++= [(verdict ($r.stderr | norm $pwd | str contains $outside) "scope: --lint-scope all reports it" "scope: --lint-scope all")]
  let r = (^$iets eval --no-tracing-cache --lint url-literal=warn --lint-root lint/scope/lib lint/scope/root/main.nix | complete)
  $res ++= [(verdict ($r.stderr | norm $pwd | str contains $outside) "scope: --lint-root adds a root" "scope: --lint-root")]
  let r = (^$iets eval --no-tracing-cache --lint url-literal=warn lint/scope/all/main.nix | complete)
  $res ++= [(verdict ($r.stderr | norm $pwd | str contains $outside) "scope: iets.toml scope = \"all\"" "scope: toml scope")]
  let r = (^$iets eval --no-tracing-cache --lint url-literal=warn lint/scope/roots/main.nix | complete)
  $res ++= [(verdict ($r.stderr | norm $pwd | str contains $outside) "scope: iets.toml roots" "scope: toml roots")]
  let r = (^$iets lint list --lint-scope all lint/scope/root/main.nix | complete)
  $res ++= [(verdict ($r.stdout | str contains "scope: all") "lint list shows the scope" "lint list scope")]
  let cache = (mktemp -d --tmpdir iets-linttest.XXXXXX)
  let r1 = (with-env {IETS_CACHE_DIR: $cache} { ^$iets eval --timings --lint url-literal=warn lint/scope/root/main.nix | complete })
  let r2 = (with-env {IETS_CACHE_DIR: $cache} { ^$iets eval --timings --lint url-literal=warn --lint-scope all lint/scope/root/main.nix | complete })
  rm -rf $cache
  let ok = (not ($r1.stderr | str contains "warning:")) and (not ($r2.stderr | str contains "cache: hit")) and ($r2.stderr | norm $pwd | str contains $outside)
  $res ++= [(verdict $ok "scope: the scope is part of the cache key" "scope: a run with another scope hit the cache")]

  # derivation attributes bound by a wrapper outside the root: the list is the
  # wrapper's contract (silent), a fixed-output hash is the user's value (named)
  let r = (^$iets eval --no-tracing-cache --lint drv-list=warn lint/scope/root/drv.nix | complete)
  $res ++= [(verdict (not ($r.stderr | str contains "warning:")) "scope: drv-list is silent through a wrapper outside the root" $"scope: drv-list through a wrapper: ($r.stderr | norm $pwd)")]
  let r = (^$iets eval --no-tracing-cache --lint fod=warn lint/scope/root/fod.nix | complete)
  let exp = "warning: [fod] derivation 'wrapped' is fixed-output (recursive, sha256)\n  at /pwd/lint/scope/root/fod.nix:1:25"
  $res ++= [(verdict (($r.stderr | norm $pwd) == $exp) "scope: fod names the user's derivation" $"scope: fod attribution: ($r.stderr | norm $pwd)")]
  let r = (^$iets eval --no-tracing-cache --lint drv-list=warn lint/scope/root/own.nix | complete)
  $res ++= [(verdict (not ($r.stderr | str contains "warning:")) "scope: the wrapper's own derivation is silent" "scope: wrapper's derivation reported")]

  # a scratch flake with a `path:` input beside it
  let work = (mktemp -d --tmpdir iets-lintscope.XXXXXX)
  cp -r lint/scope/flake/root $work
  cp -r lint/scope/flake/dep $work
  let dir = $"($work)/root"
  let hash = (^$iets eval --raw --impure --no-tracing-cache -E $"\(builtins.fetchTree { type = \"path\"; path = \"($work)/dep\"; }).narHash" | complete | get stdout | chomp)
  let store = (^readlink -f (^bash -c "command -v nu" | chomp) | chomp)
  for f in [flake.nix flake.lock] {
    let text = (open --raw $"($dir)/($f)" | str replace -a "@DEP@" $"($work)/dep" | str replace -a "@HASH@" $hash | str replace -a "@STORE@" $store)
    $text | save -f $"($dir)/($f)"
  }
  let copies = [--lint store-path-recopy=warn --lint path-copy=warn --lint dir-copy-flake=warn]
  let root_site = $"at ($dir)/flake.nix:4:25"
  let dep_site = $"at ($work)/dep/flake.nix:1:33"
  let r = (^$iets eval --raw --no-tracing-cache --lint url-literal=warn $"($dir)#value" | complete)
  $res ++= [(verdict (($r.stdout == "http://dep.examplehttp://root.example") and ($r.stderr | str contains $root_site) and not ($r.stderr | str contains $dep_site)) "flake: the input's site is silent, the root's is reported" $"flake: default scope: ($r.stderr)")]
  let r = (^$iets eval --raw --no-tracing-cache --lint url-literal=warn --lint-scope all $"($dir)#value" | complete)
  $res ++= [(verdict (($r.stderr | str contains $root_site) and ($r.stderr | str contains $dep_site)) "flake: --lint-scope all reports the input's site" $"flake: scope all: ($r.stderr)")]
  let r = (^$iets eval --raw --no-tracing-cache ...$copies $"($dir)#file" | complete)
  $res ++= [(verdict (($r.stdout | str starts-with "/nix/store/") and not ($r.stderr | str contains "warning:")) "flake: \"${./file}\" under the root is neither a re-copy nor a path-copy" $"flake: file: ($r.stderr)")]
  let r = (^$iets eval --raw --no-tracing-cache ...$copies $"($dir)#dir" | complete)
  # the message names the local directory, then the store path of the copy
  let dir_msg = $"warning: [dir-copy-flake] directory '($dir)/sub' of the flake tree is copied to the store as '/nix/store/"
  let ok = (($r.stderr | str contains $dir_msg) and ($r.stderr | str contains "-sub' by this string coercion") and ($r.stderr | str contains "the tracked files") and not ($r.stderr | str contains "[path-copy]") and not ($r.stderr | str contains "[store-path-recopy]"))
  $res ++= [(verdict $ok "flake: \"${./dir}\" under the root is dir-copy-flake, not path-copy" $"flake: dir: ($r.stderr)")]
  let r = (^$iets eval --raw --no-tracing-cache --lint dir-copy-flake=error $"($dir)#dir" | complete)
  let ok = (($r.exit_code == 1) and ($r.stderr | str contains ($dir_msg | str replace "warning:" "error:")) and ($r.stderr | str contains "-sub' by this string coercion"))
  $res ++= [(verdict $ok "flake: the dir-copy-flake error names the store path of the copy" $"flake: dir at error: ($r.stderr)")]
  let r = (^$iets eval --raw --no-tracing-cache --lint-scope all ...$copies $"($dir)#inputFile" | complete)
  $res ++= [(verdict (($r.stdout | str starts-with "/nix/store/") and not ($r.stderr | str contains "warning:")) "flake: \"${./file}\" inside an input is neither a re-copy nor a path-copy" $"flake: input file: ($r.stderr)")]
  # --impure: a pure evaluation may not read a store path it did not allow,
  # so the copy that `path-copy` reports never happens
  let r = (^$iets eval --raw --impure --no-tracing-cache ...$copies $"($dir)#store" | complete)
  $res ++= [(verdict (($r.stderr | str contains "warning: [store-path-recopy]") and ($r.stderr | str contains "warning: [path-copy] path '")) "flake: a pre-existing store path is still a re-copy" $"flake: store: ($r.stderr)")]
  "[lint.inputs.dep]\nurl-literal = \"warn\"\n" | save -f $"($dir)/iets.toml"
  let r = (^$iets eval --raw --no-tracing-cache --lint url-literal=warn $"($dir)#value" | complete)
  $res ++= [(verdict (($r.stderr | str contains $root_site) and ($r.stderr | str contains $dep_site)) "flake: [lint.inputs.dep] opens the input" $"flake: inputs table: ($r.stderr)")]

  # tree-meta-in-drv: the tree a value comes from decides the level, not the site
  rm $"($dir)/iets.toml"
  let meta = "warning: [tree-meta-in-drv] derivation 'meta': attribute"
  let r = (^$iets eval --raw --no-tracing-cache $"($dir)#meta.drvPath" | complete)
  let ok = (($r.stderr | str contains $"($meta) 'v' holds 'self.lastModifiedDate', a value that changes with every commit or edit") and ($r.stderr | str contains $"($meta) 'args' holds 'self.narHash'") and ($r.stderr | str contains $"at ($dir)/flake.nix:9:69"))
  $res ++= [(verdict $ok "tree-meta: the root's metadata in a derivation is a warning by default" $"tree-meta: root: ($r.stderr)")]
  let r = (^$iets eval --raw --no-tracing-cache --lint tree-meta-in-drv=error $"($dir)#meta.drvPath" | complete)
  $res ++= [(verdict ($r.exit_code == 1 and ($r.stderr | str starts-with "error: [tree-meta-in-drv]")) "tree-meta: --lint sets the level" $"tree-meta: error: rc=($r.exit_code) ($r.stderr)")]
  let r = (with-env {IETS_LINT: "family:purity=off"} { ^$iets eval --raw --no-tracing-cache $"($dir)#meta.drvPath" | complete })
  $res ++= [(verdict ($r.exit_code == 0 and not ($r.stderr | str contains "warning:")) "tree-meta: IETS_LINT turns it off" $"tree-meta: IETS_LINT: ($r.stderr)")]
  mkdir $"($work)/user/iets"
  "[lint]\ntree-meta-in-drv = \"error\"\n" | save $"($work)/user/iets/iets.toml"
  let r = (with-env {XDG_CONFIG_HOME: $"($work)/user"} { ^$iets eval --raw --no-tracing-cache $"($dir)#meta.drvPath" | complete })
  $res ++= [(verdict ($r.exit_code == 1 and ($r.stderr | str starts-with "error: [tree-meta-in-drv]")) "tree-meta: the user iets.toml sets the level" $"tree-meta: user toml: rc=($r.exit_code) ($r.stderr)")]
  let r = (^$iets eval --raw --no-tracing-cache $"($dir)#wrapped.drvPath" | complete)
  let ok = (($r.stderr | str contains "derivation 'wrapped': attribute 'v' holds 'self.narHash'") and ($r.stderr | str contains $"at ($work)/dep/flake.nix:2:79"))
  $res ++= [(verdict $ok "tree-meta: the root's value through a wrapper of an input is reported" $"tree-meta: wrapper: ($r.stderr)")]
  let r = (^$iets eval --raw --no-tracing-cache $"($dir)#allowed.drvPath" | complete)
  $res ++= [(verdict ($r.exit_code == 0 and not ($r.stderr | str contains "warning:")) "tree-meta: # iets: allow silences the site" $"tree-meta: allow: ($r.stderr)")]
  let r = (^$iets eval --raw --no-tracing-cache $"($dir)#depMeta.drvPath" | complete)
  $res ++= [(verdict ($r.exit_code == 0 and not ($r.stderr | str contains "warning:")) "tree-meta: the metadata of an input is silent" $"tree-meta: input: ($r.stderr)")]
  let r = (^$iets eval --raw --no-tracing-cache --lint-scope all $"($dir)#depMeta.drvPath" | complete)
  $res ++= [(verdict ($r.stderr | str contains "holds 'inputs.dep.narHash', a value that changes with every update of the lock file") "tree-meta: --lint-scope all reports the input's metadata" $"tree-meta: scope all: ($r.stderr)")]
  let cache = (mktemp -d --tmpdir iets-linttest.XXXXXX)
  let json = [--raw --timings --log-format internal-json $"($dir)#meta.drvPath"]
  let r1 = (with-env {IETS_CACHE_DIR: $cache} { ^$iets eval ...$json | complete })
  let r2 = (with-env {IETS_CACHE_DIR: $cache} { ^$iets eval ...$json | complete })
  rm -rf $cache
  let has = {|r| ($r.stderr | str contains '"code":"lint"') and ($r.stderr | str contains "[tree-meta-in-drv] derivation 'meta': attribute 'v'") and ($r.stderr | str contains '"start":') }
  $res ++= [(verdict ((do $has $r1) and (do $has $r2) and ($r2.stderr | str contains "cache: hit") and $r1.stdout == $r2.stdout) "tree-meta: a cache hit gives the finding, as a record with the span" $"tree-meta: cache: ($r1.stderr) ($r2.stderr)")]
  "[lint]\ntree-meta-in-drv = \"off\"\n[lint.inputs.dep]\ntree-meta-in-drv = \"warn\"\n" | save -f $"($dir)/iets.toml"
  let r = (^$iets eval --raw --no-tracing-cache $"($dir)#depMeta.drvPath" | complete)
  let ok = (($r.stderr | str contains "derivation 'dep-meta': attribute 'v' holds 'inputs.dep.narHash'") and ($r.stderr | str contains $"at ($dir)/flake.nix:11:76"))
  $res ++= [(verdict $ok "tree-meta: [lint.inputs.dep] reports the input's metadata" $"tree-meta: inputs table: ($r.stderr)")]
  let r = (^$iets eval --raw --no-tracing-cache $"($dir)#meta.drvPath" | complete)
  $res ++= [(verdict ($r.exit_code == 0 and not ($r.stderr | str contains "warning:")) "tree-meta: the project iets.toml turns the root off" $"tree-meta: project toml: ($r.stderr)")]
  rm -rf $work

  # `# iets: allow`: the sites on a covered line are silent
  let allow = [--lint url-literal=warn --lint eq-type-mismatch=warn]
  for f in [own-line trailing multiple family all blank-lines runtime] {
    let r = (^$iets eval --strict --no-tracing-cache ...$allow $"lint/allow/($f).nix" | complete)
    let ok = ($r.exit_code == 0 and not ($r.stderr | str contains "warning:"))
    $res ++= [(verdict $ok $"allow: ($f) is silent" $"allow: ($f): rc=($r.exit_code) ($r.stderr | norm $pwd)")]
  }
  let r = (^$iets eval --strict --no-tracing-cache --lint url-literal=warn lint/allow/next-line-only.nix | complete)
  let ok = ($r.stderr | norm $pwd | str contains "at /pwd/lint/allow/next-line-only.nix:3:3")
  $res ++= [(verdict $ok "allow: covers the next line only" $"allow: next line only: ($r.stderr | norm $pwd)")]
  let r = (^$iets eval --no-tracing-cache --lint url-literal=error lint/allow/error-level.nix | complete)
  let ok = ($r.exit_code == 0 and ($r.stdout | chomp) == "18")
  $res ++= [(verdict $ok "allow: silences a lint at error" $"allow: error level: rc=($r.exit_code) ($r.stderr)")]
  let r = (^$iets eval --no-tracing-cache --lint url-literal=warn lint/allow/directive.nix | complete)
  let ok = (($r.stderr | str contains "warning: [unknown-allow] unknown directive 'allo'") and ($r.stderr | str contains "warning: [url-literal]"))
  $res ++= [(verdict $ok "allow: an unknown directive is unknown-allow and silences nothing" $"allow: directive: ($r.stderr)")]
  # the table travels in the IR cache entry: a load stays silent
  let cache = (mktemp -d --tmpdir iets-linttest.XXXXXX)
  let ir = {IETS_CACHE_DIR: $cache, IETS_IR_MIN: "0", IETS_IR_STATS: "1"}
  for f in [own-line runtime] {
    let r1 = (with-env $ir { ^$iets eval --strict --no-tracing-cache ...$allow $"lint/allow/($f).nix" | complete })
    let r2 = (with-env $ir { ^$iets eval --strict --no-tracing-cache ...$allow $"lint/allow/($f).nix" | complete })
    let stored = ($r1.stderr | str contains $"ir-cache store ($pwd)/lint/allow/($f).nix")
    let loaded = ($r2.stderr | str contains $"ir-cache load ($pwd)/lint/allow/($f).nix")
    let ok = ($stored and $loaded and not ($r1.stderr | str contains "warning:") and not ($r2.stderr | str contains "warning:"))
    $res ++= [(verdict $ok $"allow: ($f) is silent from the IR cache" $"allow: IR cache ($f): stored=($stored) loaded=($loaded) ($r2.stderr)")]
  }
  rm -rf $cache
  # the tracing cache: a hit replays nothing, and the comment is part of the key
  let cache = (mktemp -d --tmpdir iets-linttest.XXXXXX)
  let work = (mktemp -d --tmpdir iets-lintallow.XXXXXX)
  cp lint/allow/own-line.nix $"($work)/main.nix"
  let r1 = (with-env {IETS_CACHE_DIR: $cache} { ^$iets eval --strict --timings --lint url-literal=warn $"($work)/main.nix" | complete })
  let r2 = (with-env {IETS_CACHE_DIR: $cache} { ^$iets eval --strict --timings --lint url-literal=warn $"($work)/main.nix" | complete })
  "http://example.com\n" | save -f $"($work)/main.nix"
  let r3 = (with-env {IETS_CACHE_DIR: $cache} { ^$iets eval --strict --timings --lint url-literal=warn $"($work)/main.nix" | complete })
  rm -rf $cache $work
  let hit = ($r2.stderr | str contains "cache: hit")
  let ok = ($hit and not ($r1.stderr | str contains "warning:") and not ($r2.stderr | str contains "warning:") and not ($r3.stderr | str contains "cache: hit") and ($r3.stderr | str contains $url_warn))
  $res ++= [(verdict $ok "allow: a tracing-cache hit replays nothing; the comment is in the key" $"allow: tracing cache: hit=($hit) r3=($r3.stderr)")]

  let fail = ($res | where not $it | length)
  print $"pass=($res | where $it | length) fail=($fail)"
  if $fail != 0 { exit 1 }
}
