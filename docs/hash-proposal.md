# A proposal: where hashing time really goes, and what to do about it

iets hashes for three reasons: a store path needs a digest, a path that
enters a store needs its NAR hash, and a cache entry needs a key. This says
how fast each of those is today, what the hardware allows, and which changes
are worth making. Every number here was measured, and the harness that
produced it is in the tree.

Status: the plumbing in step 4 is built. Step 1 is built in `src/store/nar.rs`
and measures 3.7x on the hashing dumps; the rest is still a proposal.

## How it was measured

Kernels and encoders: one quiet machine, one core, warm page cache. An AMD
Ryzen 9 7950X3D (Zen 4, `sha_ni` and AVX-512), pinned with `taskset -c 10`,
a core on the CCD that boosts highest. Best of 7 runs. That core's clock
moves between 4.7 and 5.54 GHz under `amd-pstate-epp`, so a kernel claim
here is in cycles, read from APERF, and a GB/s figure says which clock it
is at.

The harnesses are `examples/hash_bench.rs` (algorithms, encoders, a store
path, `mmap` against `read`, two algorithms in one pass),
`examples/sha256_mb.rs` and `examples/sha256_avx512.rs` (the multi-buffer
kernels) and `examples/nar_bench.rs` (the NAR paths). `bench/hash.nu` runs
them.

The share of an eval is the exception: it comes from `perf record` on the
kit VM, an EPYC Genoa with 16 vCPUs, because bld1 carries no `perf`. That
machine has background load, so treat those percentages as ±20% of
themselves, not as three significant figures. The same profile on a quiet
core would not move the conclusion, which is that the number is small.

Digest counts come from the `IETS_STATS=1` hash line, which this branch
adds, and are machine-independent.

## What the hardware gives

One stream, one thread, 1 MiB of input:

| Algorithm | GB/s | 32-byte call | Backend |
|:---|---:|---:|:---|
| blake3 | 9.90 | 78 ns | AVX-512 |
| sha1 | 2.92 | 51 ns | SHA-NI |
| **sha256** | **2.73** | **58 ns** | SHA-NI |
| md5 | 0.92 | 107 ns | scalar |
| sha512 | 0.91 | 210 ns | AVX2 |

Two facts in that table matter more than they look.

**BLAKE3 only wins on big inputs.** It is slower than SHA-256 below about
2 KiB: 705 ns against 428 ns at 1 KiB, and it wins at 4 KiB (949 ns against
1555 ns). Above 16 MiB it falls from 9.90 to 7.45 GB/s, because one core
cannot pull data from memory any faster. So BLAKE3 is the right choice for
file contents and the wrong one for a small cache key.

**SHA-256 is latency-bound, not throughput-bound.** A 64-byte block needs 32
`sha256rnds2`, each with 4 cycles of latency on Zen 4, and they form one
dependency chain: ~128 cycles per block. Measured with APERF, a block costs
132.7 cycles. The SHA unit is idle for about half of them. No faster
single-stream SHA-256 exists on this machine.

## Where the time really goes

Three workloads, each measured end to end.

| Workload | Total | Hashing | The rest |
|:---|---:|---:|:---|
| `eval --strict` of a NixOS toplevel | 694 ms wall, 1.63 s of CPU | 2.2% (sha256 0.9%, blake3 1.1%, encoders 0.2%) | the evaluator |
| NAR hash of one 380 MB file | 161.8 ms | 91.5% | 13.8 ms of read and copy |
| NAR hash of a 54630-file, 480 MB tree | 485.1 ms | 16% | 405.1 ms of serial walking |

The eval makes 43431 SHA-256 digests over 50.4 MB. 58% of them are under
256 bytes. So eval hashing is many small digests, and 2.2% of the CPU time
is the whole budget a hash change can take from. Section 4 took 1.0% of the
instructions out and did not move the wall clock, which is the shape of
every answer in this direction.

The big single file is the opposite: the hash is everything, and the path
already runs at 90% of the 2.73 GB/s ceiling.

The tree is the one that pays. 405 ms of its 485 ms is one thread opening,
stat-ing, reading and closing 54630 files. That thread is at the syscall
limit: `cat` of every file in one process takes 376 ms, and 73 ms in sixteen.
Proposal 1 takes that tree from 470 ms to 146 ms.

## What hashcat's book offers

hashcat optimises the same two functions, for a workload that is the mirror
image of ours: billions of tiny messages whose digests are thrown away. Its
categories are zero-based, early-exit, initial-step, precompute, reversing,
and vector width ([Optimizing computation of Hash-Algorithms as an
attacker](https://hashcat.net/events/p13/js-ocohaaaa.pdf)). Taken one at a
time:

| Trick | Verdict for iets |
|:---|:---|
| **vector width**: hash N independent messages in one instruction stream | **The one that transfers.** Measured below: 1.76x on two lanes, 2.02x on sixteen. |
| **zero-based**: fold away rounds whose message words are zero | Dead. SHA-NI computes the schedule in hardware; there is nothing to fold. |
| **initial-step / precompute**: keep the midstate of a constant prefix | Dead by volume. The only constant prefix in iets is the build id in `trace::Key::new`, and a run makes one key. It saves one block per run. |
| **early-exit**: stop as soon as the digest cannot match | Dead. We need whole digests, not a yes or no. |
| **reversing**: run the last rounds backwards from the target | Cracking only. |

### Vector width, measured

Two prototypes, both checked digest for digest against `store::hash`:
`examples/sha256_mb.rs` interleaves independent streams on the SHA-NI unit,
and `examples/sha256_avx512.rs` puts one message per 32-bit lane of a vector
register and uses ordinary add, rotate, xor and ternlog instead.

This core's clock moves between 4.7 and 5.54 GHz under `amd-pstate-epp`, so
the same kernel measures differently depending on where in a process it
runs. Cycles per 64-byte block per lane is the figure that holds; it comes
from APERF read in the harness. GB/s below is at the 5.54 GHz the kernels
actually ran at.

| Kernel | lanes needed | cycles/block/lane | GB/s |
|:---|---:|---:|---:|
| stock `sha2` 0.10, one stream | 1 | 132.7 | 2.73 |
| the same two streams in one function, not interleaved | 2 | 133 | 2.71 |
| 8-lane AVX2, no rotate and no ternlog | 8 | 128.0 | 2.77 |
| generic N-lane SHA-NI that spills | 8 | 94 | 4.35 |
| **2-lane SHA-NI, rounds hand-interleaved** | **2** | **75.5** | **4.63** |
| 8-lane AVX-512VL | 8 | 73.8 | 4.80 |
| 16-lane AVX-512 | 16 | 65.7 | 5.40 |

Three things fall out of that table.

**Interleaving has to be by hand.** Two streams written one after the other
in the same function gain nothing: 240 instructions of dependent work do not
fit in the out-of-order window.

**Two lanes of SHA-NI is the good deal.** It needs two messages and gives
1.76x. The widest vector kernel needs sixteen messages of equal block count
and gives 2.02x — only 15% more than the two-lane kernel, for eight times
the batching requirement and six times the code.

**The vector kernel is pipe-bound, not latency-bound.** Per 16-lane block
step it issues 1865 instructions, of which 672 rotates and shifts can only
go to two of the four vector pipes; the floor that implies is 1032 cycles
and it measures 1048, so the pipes are ~97% busy. Nothing is left on the
table there. The transpose is 4.7% of the instructions, and pinning it in
inline assembly changed nothing (1047.6 against 1047.9 cycles). Gathering
instead of transposing costs 13 cycles per `vpgatherdd` and drops it to
4.49 GB/s. Plain AVX2, which has no vector rotate and no ternlog, lands
exactly on the single-stream SHA-NI number and is pointless.

**A hybrid is not promising.** `sha256rnds2` serialises against `vprold`:
mixed, they issue at 1.0 per cycle instead of 2.0. An estimated ~6%.

Hashing scales across cores: 16 of these benchmarks at once run 14% slower
than one. A faster kernel multiplies by the core count.
## The proposals

### 1. Read the tree ahead of the hasher (built, 3.7x)

The dominant cost of hashing a source tree is not the hash. It is one thread
waiting on 54630 serial `open`/`read`/`close` round trips and the directory
walk around them.

`examples/nar_prefetch.rs` keeps the NAR stream exactly serial, because the
digest must be Nix's, and puts everything else beside it. Four stages run at
once: a pool reads directories breadth first from the root; one planner
thread walks those listings depth first, which is NAR order, blocking only on
a directory the pool has not reached; readers fill one slab per run of
consecutive files; and the writer replays the planner's events into the
hasher, opening nothing itself.

On the 54630-file tree, best of five:

| readers | wall | against serial |
|---:|---:|---:|
| serial | 470 ms | 1.00x |
| 4 | 215.6 ms | 2.18x |
| 8 | 166.6 ms | 2.82x |
| **16** | **145.9 ms** | **3.22x** |
| 32 | 145.6 ms | 3.23x |

Hashing 208 MB on its own is 76 ms, so the pipeline is within 2x of the only
work that cannot be removed, and it stops improving at 16 readers.

The built version (`hash_tree` in `src/store/nar.rs`) keeps the walk on the
calling thread, so a filter runs where it always did, and one pool both
lists and reads. It measures 127.2 ms on the same tree, 3.72x, with its
default of 8 pool threads (`IETS_NAR_READERS`); 16 threads cost 156 ms,
because every thread contends on one lock. The pool starts after 512
entries: a 262-entry tree pays nothing, a 700-file tree pays 1%, and 1500
files already win 1.5x.

**Two earlier shapes of this lost the whole win, which is the lesson.** One
channel message and one allocation per file costs more than the syscalls it
saves, because the buffer is allocated on one thread and freed on another:
that version ran at 1.19x. Enumerating the tree in level barriers before
streaming anything costs the whole enumeration, 87 ms of it, on the critical
path: 2.17x. Only batching the handover into slabs *and* overlapping all four
stages gets 3.2x.

What the prototype does not do, and a real implementation has to:

- **Filters.** It hashes everything. `nar_hash_filtered` takes a `PathFilter`,
  and under `builtins.filterSource` that closure is Nix code, which cannot run
  on a reader thread. The planner is the one serial walk, so the filter
  belongs there, which means the planner has to be the calling thread and the
  writer has to move off it.
- **Its own threads.** It spawns `std::thread`s. A NAR dump can happen inside
  the evaluator's own pool, so the real one must use `src/parallel` and not
  oversubscribe.
- **Memory.** It holds every listing for the whole tree, about 10 MB for
  nixpkgs, where the serial walk holds one path at a time in an arena. Slabs
  in flight add `readers * DEPTH * RUN_BYTES`, 48 MB at the defaults.
- **One stat per entry.** Enumeration stats everything, where the serial walk
  takes the type from the directory entry. More syscalls in total, but they
  are off the critical path.

Digests were checked against the serial implementation on 46 store paths and
on a fixture with empty directories, zero-length files, an executable, a file
over the prefetch limit, a dangling symlink, and a single file and an empty
directory as the root.
### 2. `mmap` the big regular files, for SHA-2 only

| 380 MB file | read loop | `mmap` |
|:---|---:|---:|
| sha256 | 2486 MB/s | **2732 MB/s** (+9.9%) |
| blake3 | **7337 MB/s** | 6762 MB/s (−7.8%) |

Mapping lets SHA-256 reach its in-memory ceiling exactly, because the copy
into the 1 MiB NAR buffer disappears. It makes BLAKE3 slower, because BLAKE3
is already memory-bound, and the 1 MiB buffer was keeping the bytes in L2.

So: map regular files above about 1 MiB when the algorithm is in the SHA-2
family, and keep the read loop for BLAKE3. Landmine: a mapping gives SIGBUS
if the file is truncated under it, so the size threshold and the existing
"file changed type" check have to cover it.

### 3. A 2-lane SHA-NI kernel, where two streams exist

`store::hash::Sha256x2`: feed two independent messages, get two digests,
1.76x. Behind a runtime `sha` check with the stock crate as the fallback.

It is only worth building together with a caller that has two streams. Today
there are few: `trace::tree_fingerprint` already runs two SHA-256 streams
side by side, and a multi-output derivation registers one NAR hash per
output. Verifying many store paths per core is the case that would really
use it.

Do not build the kernel first and hunt for callers afterwards.

### 4. The plumbing around the digest (built)

A store path costs more than its digest. Best of four runs on each side, the
same harness built against both trees, with the untouched base64 encoder as
a control:

| Per call | before | after |
|:---|---:|---:|
| base16 of 32 bytes | 45 ns | **22 ns** |
| nix32 of 20 bytes | 48 ns | **32 ns** |
| nix32 of 32 bytes | 75 ns | **47 ns** |
| `compress_hash` | 16 ns | **13 ns** |
| base64 of 32 bytes (control) | 44 ns | 44 ns |
| `make_store_path` | 178 ns | **143 ns** |
| `make_text_path` | 186 ns | **153 ns** |

The encoders pushed one `char` at a time, which re-encodes UTF-8 and checks
capacity per character; they now fill a stack buffer and append it once.
`compress_hash` allocated a `Vec` per store path; it now folds on the stack.
`base64_decode` scanned the 64-character alphabet per character; it now uses
the same digit table that nix32 got in dc4b3f59. No unsafe code, and the
digests are unchanged: 375 language and lint tests plus the unit tests pass.

End to end this is worth 1.0% of an eval's instructions: 8.348 G before,
8.262 G after, three runs each with the cold run discarded. Wall clock does
not move, because an eval's spread is 6%: 694.0 ms before against 693.9 ms
after, twelve runs each. The paths that hash big data are untouched, because
no digest encoder runs there.

So this is the right code, not a win anybody will feel. The measurable
numbers in this document are all in the proposals above.
### 5. One pass when two algorithms want the same bytes

Interleaving a SHA-256 and a BLAKE3 hasher at 64 KiB granularity, against
two separate passes:

| Data size | two passes | one pass, 64 KiB chunks |
|:---|---:|---:|
| 1 MiB (fits in L2) | 490 µs | 491 µs |
| 256 MiB | 137.6 ms | **125.9 ms** (−8.5%) |

Below the cache size there is nothing to win. Above it, one pass saves a
whole read from memory. Finer granularity is much worse: at 4 KiB the pair
costs 161 ms, because per-call overhead takes over.

So: if a byte stream needs two digests and is bigger than L2, interleave at
64 KiB. Do not interleave smaller.

### 6. Finish the accounting

This branch adds a hash line to `IETS_STATS=1`: digests, bytes and a size
histogram per algorithm. It only sees what goes through `store::hash::Hasher`.
The direct users of `sha2` and `blake3` — `trace/`, `cachefs.rs`,
`compile/cache.rs`, `fetch/spool.rs` — bypass it, so BLAKE3 does not appear
in the histogram at all. Route them through the façade, so one knob answers
how much a run hashed.

### 7. Check every algorithm choice against the 2 KiB crossover

BLAKE3 loses to SHA-256 below ~2 KiB on this machine. Where iets picks the
algorithm itself, the input size should pick it: BLAKE3 for file contents and
bodies, SHA-256 for directory listings, cache keys and fingerprints.

### 8. Speculative: hide BLAKE3 inside SHA-256's bubbles

SHA-256 holds its pipe for ~70 of the ~128 cycles a block takes. The rest is
latency, and BLAKE3 uses entirely different units. A kernel that compresses
one SHA-256 block and one BLAKE3 block in the same instruction stream could
make the second digest nearly free, which is up to 20% off a double pass.
Coarse interleaving cannot reach it: the window is too small, as the 8.5%
above shows. This needs one fused kernel and is a real piece of work. Worth
a day to find out, not more.

## Rejected, with the measurement

So nobody has to try these again.

| Idea | Why not |
|:---|:---|
| Switch the digests to `ring`, already in the tree through rustls | sha256 identical at 1 MiB (2727 against 2726 MB/s), 20% better at 64 bytes, sha512 identical (913 against 891), and its sha1 is 4.2x slower because it has no SHA-NI path. |
| Make sha512 or md5 fast | 0.91 GB/s is what AVX2 costs for sha512; `ring` matches it; Zen 4 has no SHA-512 instructions. Both are rare in Nix. Only a hand-written kernel would move them. |
| BLAKE3 instead of the boxed hasher for one-shot calls | 104.7 ns against 104.9 ns. The allocation is not the cost. |
| 16-lane AVX-512 instead of 2-lane SHA-NI | 2.02x against 1.76x, for sixteen equal-length messages at once instead of two, and 1000 lines instead of 150. The pipes are already 97% busy, so it will not get better. Revisit only if a caller turns up that really has 16 messages. |
| A SHA-NI and AVX-512 hybrid | `sha256rnds2` and `vprold` issue at 1.0 per cycle together instead of 2.0. ~6%. |
| A 3rd or 4th SHA-NI lane | 2 lanes already sit at 75.5 cycles per block against a ~64-cycle pipe floor. 8 spilling lanes gave 4.35 GB/s against 4.63 for 2 clean ones. |
| Split one NAR stream across cores | SHA-256 is serial by construction, and the digest has to be Nix's. |
| Hash a tree's files in parallel | Same reason. The NAR is one stream. Only the reads can go wide, which is proposal 1. |
