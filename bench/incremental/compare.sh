#!/usr/bin/env bash
# compare.sh: the 8-chunk evaluation of full2/batch-noq-*.nix at base (overhead2.sh makes them), three
# rounds each: CppNix, stock iets without caches, stock iets with a warm IR cache, and stock iets with
# a tracing cache hit. The first CppNix and IR-cache runs fill the OS and IR caches and count as cold.
set -u
cd ~/incr
N=/run/current-system/sw/bin/nix-instantiate
I=$HOME/iets-incr-base/target/release/iets
C=/tmp/fine-test/compare-cache
[ "$(git -C base rev-parse --short=12 HEAD)" = 9dbd131e6c65 ] || { echo "base is not at 9dbd131e6c65"; exit 1; }
exec 9> ~/bld1.lock
flock 9
run() { # label command... (@K@ is the chunk)
  local label=$1; shift
  until awk '{ exit !($1 < 1.0 && $2 < 1.5) }' /proc/loadavg; do sleep 10; done
  local t0=$(date +%s.%N)
  for k in 0 1 2 3 4 5 6 7; do
    ( /usr/bin/env time -f "%e %M" "${@//@K@/$k}" > full2/cmp-out-$k 2> full2/cmp-err-$k ) &
  done
  wait
  local wall=$(awk -v a=$t0 -v b=$(date +%s.%N) 'BEGIN { printf "%.2f", b - a }')
  if grep -q '^error' full2/cmp-err-?; then echo "$label: a chunk failed"; exit 1; fi
  local mem=$(for k in 0 1 2 3 4 5 6 7; do tail -1 full2/cmp-err-$k; done | awk '{ if ($2 > m) m = $2; s += $1 } END { printf "%.2f %.1f", s, m / 1048576 }')
  echo "$label wall $wall cpu-sum-of-chunk-walls ${mem% *} peak-GB ${mem#* }"
}
b=full2/batch-noq-@K@.nix
e="import $HOME/incr/$b"
rm -rf $C
run cppnix-first $N --eval --strict $b
for k in 0 1 2 3 4 5 6 7; do cp full2/cmp-out-$k full2/cmp-cppnix-$k; done
for r in 1 2 3; do run cppnix $N --eval --strict $b; done
for r in 1 2 3; do run iets-nocache $I eval --strict --impure --no-ir-cache --no-tracing-cache --workers 1 -E "$e"; done
run iets-ir-first env IETS_CACHE_DIR=$C $I eval --strict --impure --no-tracing-cache --workers 1 -E "$e"
for r in 1 2 3; do run iets-ir-warm env IETS_CACHE_DIR=$C $I eval --strict --impure --no-tracing-cache --workers 1 -E "$e"; done
run iets-trace-first env IETS_CACHE_DIR=$C $I eval --strict --impure --workers 1 -E "$e"
for r in 1 2 3; do run iets-trace-hit env IETS_CACHE_DIR=$C $I eval --strict --impure --workers 1 -E "$e"; done
rm -rf $C
