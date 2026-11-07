# The root-only parts of bench/ci.nu (`--privileged`, Linux): kernel
# profiling and real huge-page scarcity. Every setting changed here is
# printed, recorded with its old value and put back by `restore`, which
# also stops the fragmenter, so a failed scenario leaves nothing behind.

# The settings this module may change, with their files.
const KNOBS = {
  perf_event_paranoid: /proc/sys/kernel/perf_event_paranoid
  kptr_restrict: /proc/sys/kernel/kptr_restrict
  nmi_watchdog: /proc/sys/kernel/nmi_watchdog
  thp_defrag: /sys/kernel/mm/transparent_hugepage/defrag
  khugepaged_defrag: /sys/kernel/mm/transparent_hugepage/khugepaged/defrag
  compaction_proactiveness: /proc/sys/vm/compaction_proactiveness
  compact_unevictable_allowed: /proc/sys/vm/compact_unevictable_allowed
  nr_hugepages: /proc/sys/vm/nr_hugepages
}

# The current value of a knob: the selected word of a `[x]` list, or the
# number.
def read-knob [path: string]: nothing -> string {
  let s = (open --raw $path | str trim)
  let sel = ($s | parse -r '\[(?<v>[^\]]+)\]' | get -o 0.v)
  $sel | default $s
}

def write-knob [name: string, value: string]: nothing -> nothing {
  let path = ($KNOBS | get $name)
  print $"  sudo: ($name) = ($value)"
  $value | ^sudo -n tee $path | ignore
}

# Whole free huge-page blocks in every zone: the buddy allocator's free
# lists of the THP order and above (2 MB on x86-64 with 4 KB pages; read
# from the kernel, not assumed).
export def free-huge-blocks []: nothing -> int {
  let page = (^getconf PAGESIZE | into int)
  let hpage = (open --raw /sys/kernel/mm/transparent_hugepage/hpage_pmd_size | str trim | into int)
  let order = ((($hpage / $page) | math log 2) | math round | into int)
  open /proc/buddyinfo | lines | each {|l|
    let counts = ($l | str trim | split row -r '\s+' | skip 4 | into int)
    $counts | enumerate | where index >= $order | each {|c| $c.item * (2 ** ($c.index - $order)) } | math sum
  } | math sum
}

# Asks for sudo once, records every knob's value, and starts a root guard
# that puts them back and stops the fragmenter once this process is gone,
# however it ends (Ctrl-C, SIGTERM from a cancelled CI job, a crash). The
# guard runs outside this session and cgroup: a systemd unit of its own
# where systemd runs, else a new session (setsid), so neither a hangup nor
# a CI runner killing the job's cgroup takes it along.
export def begin [helper: string, work: string]: nothing -> record {
  if $nu.os-info.name != "linux" { error make {msg: "--privileged is Linux only"} }
  for t in [perf gawk setpriv setsid] { if (which $t | is-empty) { error make {msg: $"--privileged needs ($t) on the PATH"} } }
  # without a password if sudo allows it, else ask once
  if (^sudo -n true | complete).exit_code != 0 { ^sudo -v }
  let saved = ($KNOBS | items {|k, p| {k: $k, v: (read-knob $p)} } | reduce -f {} {|r, a| $a | insert $r.k $r.v })
  print $"privileged mode; saved: ($saved | to nuon)"
  let restore = ($work | path join restore.tsv)
  # nr_hugepages first, so the pool is gone before compaction settings return
  ([nr_hugepages] ++ ($saved | columns | where $it != nr_hugepages)) | each {|k| $"($KNOBS | get $k)\t($saved | get $k)" } | str join "\n" | save -f $restore
  let me = ($nu.pid | into string)
  let guard = [$helper guard $me $restore ($work | path join fragmenter.pid)]
  if ("/run/systemd/system" | path exists) and (which systemd-run | is-not-empty) {
    ^sudo -n systemd-run --quiet --collect --unit $"iets-ci-guard-($me)" -- ...$guard
  } else {
    ^sudo -n setsid -f ...$guard o+e>| /dev/null
  }
  # no guard, no privileged mode: nothing would undo the settings
  mut tries = 0
  # the guard itself, not the sudo or setsid that starts it
  while (^pgrep -f $"^($helper) guard ($me) " | complete).exit_code != 0 {
    if $tries >= 50 { error make {msg: "the root guard did not start"} }
    sleep 100ms
    $tries += 1
  }
  # keep the sudo timestamp alive for hours of scenarios, until this process ends
  ^sh -c $"while kill -0 ($me) 2>/dev/null; do sudo -n -v 2>/dev/null; sleep 60; done >/dev/null 2>&1 &"
  # every PMU counter and kernel symbol for perf; the NMI watchdog holds a counter
  write-knob perf_event_paranoid "-1"
  write-knob kptr_restrict "0"
  write-knob nmi_watchdog "0"
  $saved
}

# Stops the fragmenter if one runs (its pid file is in `work`).
def stop-fragmenter [work: string]: nothing -> nothing {
  let pidfile = ($work | path join fragmenter.pid)
  if not ($pidfile | path exists) { return }
  let pid = (open --raw $pidfile | str trim)
  if $pid != "" and ($"/proc/($pid)" | path exists) {
    ^sudo -n kill $pid
    # it frees tens of GB on exit; the next run must not share the CPU with that
    while ($"/proc/($pid)" | path exists) { sleep 100ms }
  }
  ^sudo -n rm -f $pidfile
}

# Puts back every knob that differs from what `begin` saved, and stops the
# fragmenter.
export def restore [saved: record, work: string]: nothing -> nothing {
  stop-fragmenter $work
  for k in ($saved | columns) {
    let path = ($KNOBS | get $k)
    if (read-knob $path) != ($saved | get $k) { write-knob $k ($saved | get $k) }
  }
}

# Huge pages scarce like on a long-running machine. The fragmenter takes
# the free memory as two halves whose pages interleave physically, locks
# one and frees the other; with compaction barred from moving locked pages
# and from running proactively, no 2 MB block can be rebuilt. The blocks
# still whole are counted and exactly that many are reserved as hugetlb
# pages, so the kernel need not compact anything to fill the pool. `need`
# is the bytes the workload must still find free.
export def scarce-begin [helper: string, work: string, need: float]: nothing -> record {
  print "making huge pages scarce"
  write-knob thp_defrag never
  write-knob khugepaged_defrag "0"
  write-knob compaction_proactiveness "0"
  write-knob compact_unevictable_allowed "0"
  "3" | ^sudo -n tee /proc/sys/vm/drop_caches | ignore
  let free_mib = (open /proc/meminfo | lines | where $it starts-with "MemFree" | get 0 | parse -r '(?<kb>\d+)' | get 0.kb | into int | $in // 1024)
  # the fragmenter frees half of what it takes; that half is the workload's.
  # While it fills, the rest of the machine has only the margin: 2 GiB or
  # 5 % of memory. If something needs more then, the OOM killer takes the
  # largest process, the fragmenter, and the scarce row fails; while a
  # scenario runs, the workload goes first (helper run --oom-first).
  let total_mib = (open /proc/meminfo | lines | where $it starts-with "MemTotal" | get 0 | parse -r '(?<kb>\d+)' | get 0.kb | into int | $in // 1024)
  let mib = ($free_mib - ([2048 ($total_mib // 20)] | math max))
  if ($mib / 2 * 1048576) < ($need * 1.2) {
    error make {msg: $"not enough memory to fragment: ($free_mib) MiB free, the workload needs (($need / 1048576) | math round) MiB"}
  }
  let ready = ($work | path join fragmenter.out)
  let pidfile = ($work | path join fragmenter.pid)
  ^sh -c $"sudo -n '($helper)' fragment ($mib) '($pidfile)' > '($ready)' 2>&1 &"
  # touching tens of GB page by page takes a while on a large machine
  mut waited = 0
  loop {
    let out = (try { open --raw $ready } catch { "" })
    if $out =~ "ready" { break }
    if $out =~ "helper:|sudo:" { error make {msg: $"the fragmenter failed: ($out | str trim)"} }
    if $waited >= 6000 { error make {msg: "the fragmenter did not get ready in 20 minutes"} }
    sleep 200ms
    $waited += 1
  }
  let whole = (free-huge-blocks)
  write-knob nr_hugepages ($whole | into string)
  let reserved = (open --raw /proc/sys/vm/nr_hugepages | str trim | into int)
  let left = (free-huge-blocks)
  print $"  fragmenter holds ($mib // 2) MiB locked; ($whole) whole huge-page blocks were free, ($reserved) reserved, ($left) left"
  {reserved: $reserved, whole_before: $whole, free_2m_blocks: $left}
}

# Whether huge pages were still scarce at the end of a scenario: the
# fragmenter alive, and no more than 64 whole blocks (128 MB) free beyond
# those left at the start. Checked before `scarce-end` undoes it.
export def scarce-held [work: string, start: record]: nothing -> record {
  let pid = (try { open --raw ($work | path join fragmenter.pid) | str trim } catch { "" })
  let alive = $pid != "" and ($"/proc/($pid)" | path exists)
  let free = (free-huge-blocks)
  let held = $alive and $free <= ($start.free_2m_blocks + 64)
  {held: $held, why: (if not $alive { "the fragmenter died (killed?), so huge pages came back" } else if not $held { $"($free) whole huge-page blocks were free at the end, ($start.free_2m_blocks) at the start" } else { "" }), free_2m_blocks_end: $free}
}

export def scarce-end [saved: record, work: string]: nothing -> nothing {
  write-knob nr_hugepages ($saved.nr_hugepages)
  stop-fragmenter $work
  for k in [thp_defrag khugepaged_defrag compaction_proactiveness compact_unevictable_allowed] {
    write-knob $k ($saved | get $k)
  }
}

# The awk that splits a `perf script` header into the thread's name (which
# may hold spaces) and the rest: the name ends where the pid begins.
const COMM = '
  function comm_of(line,   c) {
    if (match(line, /[ \t]+[0-9]+(\/[0-9]+)?[ \t]+(\[[0-9]+\][ \t]+)?[0-9]+\.[0-9]+:/)) c = substr(line, 1, RSTART - 1)
    else { c = line; sub(/[ \t].*/, "", c) }
    sub(/^[ \t]+/, "", c)
    gsub(/;/, ",", c)
    return c
  }
  function frame_of(line,   f) {
    f = line; sub(/^[ \t]+[0-9a-f]+ /, "", f); sub(/ \([^()]*\)$/, "", f); sub(/\+0x[0-9a-f]+$/, "", f)
    gsub(/;/, ",", f)
    return f
  }
'

# The stacks of a perf.data folded to `comm;root;...;leaf count` lines, the
# input of flamegraph.pl and inferno-flamegraph. perf script's text goes
# straight to gawk: for whole nixpkgs it runs to gigabytes.
def fold [data: string, out: string]: nothing -> nothing {
  ^perf script -i $data | ^gawk ($COMM + '
    /^[^ \t]/ { if (stack != "") c[comm stack]++; comm = comm_of($0); stack = ""; next }
    /^[ \t]+[0-9a-f]+ / { stack = ";" frame_of($0) stack; next }
    /^$/ { if (stack != "") c[comm stack]++; stack = "" }
    END { if (stack != "") c[comm stack]++; for (k in c) print k, c[k] }
  ') | save -f $out
}

# Off-CPU time from system-wide sched_switch events (`perf record -a`): a
# thread's stack when it left the CPU, weighted by the microseconds until
# it ran again, for the threads whose name starts with `prefix`. System
# wide, because a thread woken onto an idle CPU is switched in by the idle
# task, an event a recording of the process alone never sees. The folded
# format offcputime (bcc) writes.
def fold-offcpu [data: string, out: string, prefix: string]: nothing -> nothing {
  ^perf script -i $data | ^gawk -v $"prefix=($prefix)" ($COMM + '
    function finish() {
      if (pend != "") { out_t[pend] = t; out_s[pend] = pcomm stack }
      pend = ""; stack = ""
    }
    /^[^ \t]/ {
      finish()
      if (!match($0, /([0-9]+\.[0-9]+): sched:sched_switch: (.*):([0-9]+) \[[0-9]+\] [^ ]+ ==> (.*):([0-9]+) \[[0-9]+\]/, m)) next
      t = m[1] + 0
      if (m[5] in out_t) { c[out_s[m[5]]] += (t - out_t[m[5]]) * 1000000; delete out_t[m[5]]; delete out_s[m[5]] }
      if (index(m[2], prefix) == 1) { pend = m[3]; pcomm = m[2]; gsub(/;/, ",", pcomm) }
      next
    }
    /^[ \t]+[0-9a-f]+ / { if (pend != "") stack = ";" frame_of($0) stack; next }
    /^$/ { finish() }
    END { finish(); for (k in c) if (c[k] >= 1) printf "%s %d\n", k, c[k] }
  ') | save -f $out
}

# Where off-CPU time went: the deepest iets frame of each stack (the code
# that blocked; else the deepest Rust frame) and the system call it blocked
# in, by share of the time.
def top-offcpu [folded: string, n: int]: nothing -> table {
  let rows = (open --raw $folded | lines | parse -r '^(?<stack>.*) (?<n>\d+)$' | update n { into int })
  if ($rows | is-empty) { return [] }
  let total = ($rows | get n | math sum)
  if $total == 0 { return [] }
  $rows | each {|r|
    let frames = ($r.stack | split row ";")
    let code = ($frames | where $it =~ "iets::" | last | default ($frames | where $it =~ "::" | last | default ($frames | get 0)))
    let sys = ($frames | where $it =~ '^__(x64|arm64|ia32|se)_sys_' | get -o 0 | default "" | str replace -r '^__(x64|arm64|ia32|se)_sys_' "")
    {where: (if $sys == "" { $code } else { $"($code) \(($sys)\)" }), n: $r.n}
  }
  | group-by where | items {|k, v| {function: $k, share: (($v | get n | math sum) * 100 / $total | math round --precision 1)} }
  | sort-by share --reverse | first $n
}

# The heaviest leaf functions of a folded file, as a table.
def top-leaves [folded: string, n: int]: nothing -> table {
  let rows = (open --raw $folded | lines | parse -r '^(?<stack>.*) (?<n>\d+)$' | update n { into int })
  if ($rows | is-empty) { return [] }
  let total = ($rows | get n | math sum)
  if $total == 0 { return [] }
  $rows | each {|r| {leaf: ($r.stack | split row ";" | last), n: $r.n} }
  | group-by leaf | items {|k, v| {function: $k, share: (($v | get n | math sum) * 100 / $total | math round --precision 1)} }
  | sort-by share --reverse | first $n
}

# One profiled run of `bin` per view: counters, an on-CPU profile and the
# off-CPU stacks (system-wide sched_switch as root, the workload dropped
# back to this user). `prepare` returns the cache directory for the next
# run in the scenario's own state (fresh, warmed, or after its edit), so
# every view measures what the timed runs did. Writes the report's files
# (counters, folded stacks, flame graphs) into `dir` and perf's recordings,
# which run to GB for whole nixpkgs, into `data`, outside the checkout.
export def profile [helper: string, bin: string, args: list<string>, dir: string, data: string, prepare: closure, frame_pointers: bool]: nothing -> record {
  mkdir $dir
  mkdir $data
  # the cache stores on the command line, as for the timed runs
  let cached = {|c| [($args | first) --cache-dir $c ...($args | skip 1)] }
  # a profile is of the work, teardown included: a binary that forks to
  # exit early waits here (#370), so perf follows one process
  let envs = {|cache| {XDG_CACHE_HOME: $cache, IETS_WAIT_EXIT: "1", IETS_PROGRESS: "false"} }
  # a profiled run that fails must not pass for a profile
  let check = {|r, what| if $r.exit_code != 0 { error make {msg: $"the ($what) run failed \(exit ($r.exit_code)\): ($r.stderr | lines | last 5 | str join ' / ')"} } }
  let events = "task-clock,cycles,instructions,branches,branch-misses,cache-references,cache-misses,page-faults,minor-faults,major-faults,context-switches,cpu-migrations,dTLB-loads,dTLB-load-misses"
  let c = (do $prepare)
  do $check (with-env (do $envs $c) { ^perf stat -x ";" -e $events -o ($dir | path join stat.csv) -- $bin ...(do $cached $c) | complete }) "perf stat"
  let counters = (open --raw ($dir | path join stat.csv) | lines | where {|l| not ($l starts-with "#") and $l != "" }
    | each {|l| let f = ($l | split row ";"); {event: ($f | get 2), value: ($f | get 0)} }
    | where value !~ "not" | reduce -f {} {|r, a| $a | insert $r.event ($r.value | into float) })
  let ipc = if "cycles" in $counters and "instructions" in $counters { $counters.instructions / $counters.cycles } else { null }
  # on-CPU: frame pointers when the binary has them, else DWARF at a lower rate
  let cg = if $frame_pointers { [--call-graph fp -F 999] } else { [--call-graph "dwarf,16384" -F 199] }
  let c = (do $prepare)
  do $check (with-env (do $envs $c) { ^perf record -q -g ...$cg -o ($data | path join oncpu.data) -- $bin ...(do $cached $c) | complete }) "on-CPU"
  fold ($data | path join oncpu.data) ($dir | path join oncpu.folded)
  # off-CPU: every sched_switch on the machine, with stacks
  let u = (^id -u | str trim)
  let g = (^id -g | str trim)
  let cache = (do $prepare)
  # started through a link named iets, so its threads are iets and
  # iets-wN whatever the binary's file is called (thread names hold 15
  # characters)
  let link = ($dir | path join bin iets)
  mkdir ($dir | path join bin)
  ^ln -sf $bin $link
  # the timed runs' environment, not the one sudo resets to, through a
  # file only this user can read: on sudo's command line every value
  # (tokens, credentials) would go to its log and to ps. Lists (PATH) as
  # the OS sees them, joined with the separator.
  let kv = ($env | merge (do $envs $cache) | items {|k, v| {k: $k, v: $v} }
    | where {|e| $e.k !~ '^(PWD|OLDPWD|FILE_PWD|CURRENT_FILE|_)$' }
    | each {|e| match ($e.v | describe | str replace -r '<.*' '') {
        "string" => $"($e.k)=($e.v)"
        "list" => (if ($e.v | all {|x| ($x | describe) == "string" }) { $"($e.k)=($e.v | str join (char esep))" } else { null })
        _ => null
      } } | compact)
  # in the per-user runtime directory where there is one (memory, gone at
  # logout), so even an interrupted run leaves no copy in the checkout
  let envfile = ($env.XDG_RUNTIME_DIR? | default $data | path join $"iets-ci-env-($nu.pid)")
  $kv | str join (char nul) | ^sh -c 'umask 077; cat > "$1"' sh $envfile
  let r = (^sudo -n perf record -q -a -e sched:sched_switch -g -o ($data | path join offcpu.data) -- setpriv $"--reuid=($u)" $"--regid=($g)" --init-groups $helper run --env-file $envfile --stderr ($dir | path join offcpu.stderr) -- $link ...(do $cached $cache) | complete)
  rm $envfile
  do $check $r "off-CPU"
  let m = (try { $r.stdout | lines | where $it starts-with "{" | last | from json } catch { error make {msg: $"the off-CPU run printed no result: ($r.stderr | str trim)"} })
  if $m.exit != 0 or $m.signal != 0 { error make {msg: $"the off-CPU run failed \(exit ($m.exit), signal ($m.signal)\): (open --raw ($dir | path join offcpu.stderr) | lines | last 5 | str join ' / ')"} }
  ^sudo -n chown $"($u):($g)" ($data | path join offcpu.data)
  fold-offcpu ($data | path join offcpu.data) ($dir | path join offcpu.folded) iets
  for kind in [oncpu offcpu] {
    let f = ($dir | path join $"($kind).folded")
    if (which inferno-flamegraph | is-not-empty) {
      open --raw $f | ^inferno-flamegraph | save -f ($dir | path join $"($kind).svg")
    } else if (which flamegraph.pl | is-not-empty) {
      ^flamegraph.pl $f | save -f ($dir | path join $"($kind).svg")
    }
  }
  {
    counters: $counters, ipc: $ipc
    oncpu_top: (top-leaves ($dir | path join oncpu.folded) 12)
    offcpu_top: (top-offcpu ($dir | path join offcpu.folded) 8)
    stacks: (if $frame_pointers { "frame pointers at 999 Hz" } else { "DWARF at 199 Hz; build with RUSTFLAGS='-C force-frame-pointers=yes' and pass --frame-pointers for full stacks at 999 Hz" })
  }
}
