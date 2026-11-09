# Measure a performance change

`bench/ci.nu` compares two iets binaries on the work a CI pipeline gives
iets, and says whether the second one is slower, faster or the same, with
the evidence for it. Use it before you claim a change is faster, and
before you merge one that touches the evaluator, the arenas or the
scheduler.

```sh
nix develop
cargo build --release && cp target/release/iets /tmp/iets-new
git stash && cargo build --release && cp target/release/iets /tmp/iets-old && git stash pop
bench/ci.nu --baseline /tmp/iets-old --candidate /tmp/iets-new --tier quick  # minutes
bench/ci.nu --baseline /tmp/iets-old --candidate /tmp/iets-new               # scale: + whole nixpkgs, about half an hour (26 minutes on a Ryzen 9 7950X)
bench/ci.nu --baseline /tmp/iets-old --candidate /tmp/iets-new --tier full   # the nightly matrix, hours
```

It writes `bench/results/ci/DATE-HOST/report.md` and `results.json` (every
run, with its evidence), and `rows.jsonl` row by row as it goes, so an
interrupted run keeps what it finished. It exits 1 when a scenario got
slower or used more memory with its whole interval past the threshold
(see below for how small a change that catches), when the two
binaries printed different output (`--expect-output-change` says the
candidate does that on purpose, and the times decide), or when a binary's
output changed from one run to the next. Its working files (caches, about 200 MB per
`scale` run, and a writable nixpkgs copy for the one-file scenario, kept
for the next run) are under `~/.cache/iets-ci`. It deletes nothing there
and warns once the directory passes 5 GB, so a nightly job should clear
it and old `bench/results/ci` reports itself.

Each run gets its own cache directory on the command line (`--cache-dir`,
which wins over `IETS_CACHE_DIR` and a `cache-dir` setting), so a cold
run is cold and the two binaries never share a cache; iets before
September 2026 lacks the flag. The suite clears every `IETS_*` variable
for both binaries and lists them in the report, with the Nix settings
variables (`NIX_CONFIG`, `XDG_CONFIG_HOME`, ...) that still apply to
both.

Wall time ends when the binary exits, as its caller sees it: a binary that
exits early and frees its heap in a child (#370, not merged yet) gets the
credit. The child's CPU time still counts, and the next run starts only
after it is gone (on Linux the helper adopts it as subreaper). How long
it went on is the teardown column, and a finding names a candidate that
leaves noticeably more of it than the baseline, since the next step of a
CI job shares the machine with it. What still runs ten minutes after the
exit is killed and the run is FAILED. Profiles set `IETS_WAIT_EXIT=1`,
so they show the teardown inside the process.

Every run's output is hashed line by line, in any order, error lines and
their text included. Only the numbers in an eval-jobs `stats` object are
set aside: `wallMs` is time, and `allocBytes` counts the arena, which a
change may move on purpose. Whether `stats` is there still counts, since
nix-eval-jobs writes it and the default output must match Nix's
([Trace a store path back to its source](../explanation/provenance.md)).
A candidate that prints something else, say because it evaluates less, is
`OUTPUT DIFFERS` instead of a timing verdict, with its times still in the
row. Evaluation is deterministic by design
([How iets evaluates](../explanation/evaluation.md#determinism)), so a
binary whose own output changes from round to round is `NONDETERMINISTIC`,
and fails even an A/A run; the row names which binary. The one-file
scenario changes its input every round, so there only the two binaries of
a round are compared.

## What it measures

| task | scenarios |
|---|---|
| small steps | `eval --expr 1` (startup), one package's `drvPath`, cold and from the tracing cache |
| NixOS configs | nixos-minimal, nixos-server, nixos-plasma: no caches, and a warm IR cache |
| cached reruns | the same evaluation again (tracing-cache hit), and after a one-file change in nixpkgs |
| job sets | `eval-jobs` on `nixos/release-small.nix`, cold and cached: the aggregate job `tested`, all eval-jobs walks there by default; `full` adds every job (`--force-recurse`, 112 jobs, about 50 GB peak RSS) |
| whole nixpkgs (`scale`) | `eval --strict` of every job at 1 and 8 workers, `eval-jobs` in one process at 8; `full` adds 4 workers and worker processes |

Each scenario runs the two binaries in turn, round after round, so drift
on the machine hits both alike; with an odd number of rounds one order
runs once more. Before every run it waits until nothing else runs. A
scenario without caches gets one untimed warm-up run per binary in each
state, except cold-files, where every run starts from disk; the scenarios
with warm caches or an edit run in the plenty state only, after one
untimed run per binary that fills its cache. The change is the median of
the per-round ratios, with a distribution-free interval for it (order
statistics, no assumption about the noise's shape) whose coverage the
report states: 94 % at the 5 rounds a whole nixpkgs scenario runs at
least, 97 % at the 6 of the others. When that interval is wider than ±1 %
(`--target`) or straddles the threshold, the scenario goes on to the next
round count where the interval can narrow: 9, 12, then 15 rounds (whole
nixpkgs stops at 9). The interval is the lowest and highest ratio up to
8 rounds, so a round in between could only widen it.

## Machine states

The same binaries can differ by 10 % or more between two states of the
same machine. iets's arenas fault in 4 KB pages when the kernel has no
huge pages to give, and on a machine that has run for a while it often
has none. So the states are part of the result:

| state | how |
|---|---|
| plenty | as the machine is; the report says how many huge pages the runs got |
| thp-off | huge pages off for the measured process (`PR_SET_THP_DISABLE`): every page a 4 KB fault, as when the kernel cannot fill them |
| scarce | `--privileged`: compaction stopped, free memory fragmented, whole 2 MB blocks reserved: a long-running machine |
| cpus-2, cpus-4 | the process pinned to 2 or 4 CPUs, one per physical core where there are enough cores and CPU 0's core last (it takes most interrupts), like small CI runners; iets picks its default worker count from them. Skipped when fewer CPUs are allowed; the report lists the CPUs used |
| cold-files | nixpkgs (a symlink to it followed) dropped from the page cache before each run, like a fresh checkout. If the runs then read under 1 MB from disk, the eviction did not take and the row is n/a. Skipped on ZFS, whose cache ignores the request |
| memcap | the baseline's own cgroup peak (`memory.peak`) plus 10 % as a limit (`systemd-run --user`): does each binary still finish? Checked first that the limit really kills; reported as n/a where it cannot |

On darwin only the plenty state runs.

## Reading the report

Each row gives the median wall time with its range, the change with its
interval and the interval's coverage, and what the runs did:

- **busy CPUs**: CPU time (user and system) divided by wall time. A run
  on 4 workers that keeps 1.2 CPUs busy is waiting on something.
- **run-queue**: the time threads were runnable but waited for a CPU, as
  a share of their CPU time. Above a few per cent, another process, more
  workers than CPUs, or the scheduler is in the way.
- **huge pages**: the share of huge-page faults that got one. The
  counters are machine-wide (`/proc/vmstat`); on the idle machine the
  suite waits for, they are this run's.
- **CoV**: the runs' coefficient of variation. A change smaller than this
  is not evidence.
- **foreign CPU**: CPU seconds other processes used during a run: those
  seen in `/proc` at its start and end, plus what the machine's total
  (`/proc/stat`) shows beyond them and the run itself, which is processes
  that came and went (a parallel build, say). The total counts in 10 ms
  ticks per CPU, so that part counts only above the rounding.

Under each table the report names what limited a scenario, matched
against iets's known issues: huge pages scarce in the plenty state, page
faults per GB, run-queue wait, memory pressure, major faults, a run that
started on a busy machine.

The machine section lists settings that add noise: a CPU governor other
than `performance`, boost, a `sched_ext` scheduler. Results from two
machines, or from one machine in two states, are not comparable; compare
the change within one report.

To see how small a change a machine can show, run the same binary as
baseline and candidate (A/A). The changes it reports are the noise. On a
Ryzen 9 7950X the `scale` tier took 26 minutes and found all 26
scenario states unchanged. The intervals were within -1.1 to +0.5 % for
whole nixpkgs (5 rounds, 94 % coverage), within ±0.9 % for the
one-worker NixOS evaluations (97 %), up to [-3.0, -0.2] % at four and
eight workers, and ±2-7 % for the steps under 100 ms. 4 of the 26
intervals left out zero where about 1 was expected: interleaved runs on
one machine are not quite independent (it drifts), so the intervals run
a little narrow. The suite also looks at an interval up to four times
(at 5 or 6, 9, 12 and 15 rounds) and stops when it is narrow, which
narrows them further. That is why a verdict needs the whole interval past
the threshold, not just zero, and why the stated coverage is an upper
bound.

That rule makes the gate conservative. Simulated with independent noise
per run, the share of runs in which a true change fails the gate:

| true change | whole nixpkgs, 0.4 % noise | whole nixpkgs, 0.6 % | whole nixpkgs, 1 % | other scenarios, 1 % |
|---|---|---|---|---|
| +3 % | 4 % | 4 % | 3 % | 4 % |
| +4 % | 95 % | 73 % | 34 % | 50 % |
| +5 % | 100 % | 100 % | 82 % | 97 % |

Whole nixpkgs measured 0.4 to 0.6 % here; it then takes 5.3 to 6.8 rounds
on average when nothing changed. So the gate catches changes of about
5 % and more. Smaller ones show in the change column and its interval,
where a reviewer has to read them.

## Privileged mode

`--privileged` (Linux, asks for sudo once; needs `perf`, `gawk`,
`setpriv` and `setsid` on the PATH) adds the scarce state and a profile of
both binaries per scenario. It changes kernel settings for the run, prints
each change and restores them all at the end, also when a scenario fails;
their old values are in `results.json`. A root guard process also puts
every changed setting back and stops the fragmenter once the suite is
gone: after Ctrl-C, a cancelled CI job (SIGTERM), a crash, or a runner
that kills the job's whole cgroup. It runs as a systemd unit of its own
(else in a new session), outside the suite's session and cgroup. After
Ctrl-C nushell stops running the suite's own cleanup, so there the guard
does it. The settings to restore are on the guard's command line from
the start, and it finds the fragmenter in `/proc`: no file in the work
directory, which anything running as you could rewrite, decides what
root writes or kills.

**The scarce state takes almost all free memory.** The fragmenter fills
everything but 2 GiB (or 5 % of memory, if more), page by page, which
takes a minute or two on 64 GB; then it frees half. While it fills,
anything that allocates more than the margin makes the kernel's OOM
killer end the largest process. That is the fragmenter, and the scarce
row fails, but on a machine where a browser or an IDE is open, close
them, or run `--privileged` only on a dedicated machine. While a scenario
runs, the workload is the OOM killer's first choice (`oom_score_adj`
1000), not the desktop. A scarce row
counts only if the fragmenter was still alive at the scenario's end and
no more than 64 whole huge-page blocks had come back; otherwise it is
FAILED. The off-CPU run gets the timed runs' environment through a file
only you can read, in `$XDG_RUNTIME_DIR` (memory, gone at logout), not on
sudo's command line, where sudo would log every value and `ps` would show
it. A profiled run that fails fails its profile
row.

Per scenario, the report gets `perf stat` counters (IPC, branch, cache
and dTLB misses, page faults, context switches) and the heaviest
functions on and off the CPU. Folded stacks for flame graphs are under
`profiles/`. perf's recordings stay in the work directory
(`~/.cache/iets-ci/work-*/perf`), outside the checkout: the system-wide
`sched_switch` stacks of a whole-nixpkgs run come to GB. With `inferno-flamegraph` or `flamegraph.pl` on the PATH it
renders them. For full stacks, build both binaries with frame pointers
and say so:

```sh
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release
bench/ci.nu --baseline ... --candidate ... --privileged --frame-pointers
```

Without frame pointers it unwinds with DWARF at a low sample rate.
