# The static build

`nix build .#static` gives you a musl binary with no runtime dependencies,
for x86_64-linux or aarch64-linux. It needs no `/nix/store`, no daemon, no
shared libraries and no CA bundle.
It evaluates, fetches `github:`, `gitlab:`, `sourcehut:` and `tarball`
inputs, and builds on a `grpc://` builder. Stripped and compressed with
`xz -9`, it was 3.1 MB when last measured.

It runs `git` for a project that is a git checkout and for a `git` input. If the machine has no CA bundle, the binary trusts the
Mozilla roots built into it, and `NIX_SSL_CERT_FILE` overrides them
([Settings](../reference/settings.md)).

For a build that runs in a browser, see [WebAssembly](webassembly.md).

## The allocator

The evaluator keeps its values in its own arena, and everything else comes
from the global allocator. With musl's malloc the static build ran 24–30%
slower than the glibc build, and 79% slower on server w4 (the nixos-server
workload at four workers; `w1` is one worker).

The table times each allocator with hyperfine (1 warmup, 5 runs, median) on
the `bench/ab.nu` workloads, on an idle Ryzen 9 7950X3D pinned to cores 0–7.
The baseline is glibc's malloc.

| variant | server w1 | server w4 | minimal w1 | outpaths 0/8 w1 | outpaths 0/8 w4 |
|---|---:|---:|---:|---:|---:|
| glibc, system | 0.97 s | 0.47 s | 0.74 s | 24.4 s | 9.05 s |
| glibc, mimalloc | −7% | −7% | −8% | −8% | −7% |
| glibc, jemalloc | −4% | −4% | −5% | −7% | −7% |
| glibc, tcmalloc | −3% | −4% | −4% | −4% | −5% |
| glibc, rpmalloc | −3% | −2% | −3% | −6% | −7% |
| glibc, snmalloc | −2% | −1% | −2% | −2% | −6% |
| glibc, scudo | +5% | +3% | +4% | +4% | −1% |
| glibc, dlmalloc | +3% | +83% | +3% | +2% | +1% |
| glibc, rlsf | +4% | +93% | +4% | +3% | +1% |
| musl, mimalloc | +3% | +5% | 0% | +3% | +3% |
| musl, jemalloc | +7% | +10% | +5% | +4% | +4% |
| musl, dlmalloc | +12% | +70% | +10% | +12% | +12% |
| musl, rlsf | +12% | +78% | +10% | +11% | +10% |
| musl, system | +26% | +79% | +24% | +30% | +29% |

On musl, rpmalloc crashes at start and snmalloc does not link. tcmalloc and
scudo link only as non-PIE, and were not faster than mimalloc. mimalloc is
the fastest on both libcs, so iets uses it on every target.

Even with mimalloc the static build is 9–13% slower than the glibc build.
Most of the gap is in musl's `memcpy`, `memmove`, `memcmp` and `memset`,
which take 13.8% of the cycles on server w1, where glibc's take 2.9%.
