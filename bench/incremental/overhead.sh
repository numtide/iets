#!/usr/bin/env bash
# overhead.sh: the 8-chunk full evaluation at base, three rounds of: the branch base binary, the branch
# binary with the trace off, the trace on without a graph, and the trace on with a graph.
set -u
cd ~/incr
B=$HOME/incr/base
exec 9> ~/bld1.lock
flock 9
run() { # label binary env...
  local label=$1 bin=$2; shift 2
  until awk '{ exit !($1 < 1.0 && $2 < 1.5) }' /proc/loadavg; do sleep 10; done
  local t0=$(date +%s.%N)
  for k in 0 1 2 3 4 5 6 7; do
    ( rm -rf full/ov-dag-$k
      env "${@//@K@/$k}" /usr/bin/env time -f "%e %M" $bin eval --strict --impure --no-ir-cache --no-tracing-cache --workers 1 \
        -E "import $HOME/incr/full/batch-$k.nix" 2> full/ov-err-$k > /dev/null ) &
  done
  wait
  local wall=$(awk -v a=$t0 -v b=$(date +%s.%N) 'BEGIN { printf "%.2f", b - a }')
  local mem=$(for k in 0 1 2 3 4 5 6 7; do tail -1 full/ov-err-$k; done | awk '{ if ($2 > m) m = $2; s += $1 } END { printf "%.2f %.1f", s, m / 1048576 }')
  echo "$label wall $wall cpu-sum-of-chunk-walls ${mem% *} peak-GB ${mem#* }"
}
for r in 1 2 3; do
  run base $HOME/iets-incr-base/target/release/iets X=1
  run off $HOME/iets-incr/target/release/iets X=1
  run record $HOME/iets-incr/target/release/iets IETS_FINE_TRACE=
  run graph $HOME/iets-incr/target/release/iets IETS_FINE_TRACE= IETS_FINE_DAG=full/ov-dag-@K@
done
rm -rf full/ov-dag-*
