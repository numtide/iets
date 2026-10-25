#!/usr/bin/env bash
# overhead2.sh: the 8-chunk evaluation of full2/batch-*.nix at base, three rounds of: the branch base
# binary, the branch binary with the trace off, the trace on without queries (no fineRoot), the trace
# on with queries, and the trace on with queries and a graph.
set -u
cd ~/incr
for k in 0 1 2 3 4 5 6 7; do
  sed 's/^builtins.seq (builtins.fineRoot pkgs) \[$/[/' full2/batch-$k.nix > full2/batch-noq-$k.nix
done
[ "$(git -C base rev-parse --short=12 HEAD)" = 9dbd131e6c65 ] || { echo "base is not at 9dbd131e6c65"; exit 1; }
exec 9> ~/bld1.lock
flock 9
run() { # label binary batch env...
  local label=$1 bin=$2 batch=$3; shift 3
  until awk '{ exit !($1 < 1.0 && $2 < 1.5) }' /proc/loadavg; do sleep 10; done
  local t0=$(date +%s.%N)
  for k in 0 1 2 3 4 5 6 7; do
    ( rm -rf full2/ov-dag-$k
      env "${@//@K@/$k}" /usr/bin/env time -f "%e %M" $bin eval --strict --impure --no-ir-cache --no-tracing-cache --workers 1 \
        -E "import $HOME/incr/full2/$batch-$k.nix" 2> full2/ov-err-$k > /dev/null ) &
  done
  wait
  local wall=$(awk -v a=$t0 -v b=$(date +%s.%N) 'BEGIN { printf "%.2f", b - a }')
  if grep -q '^error' full2/ov-err-?; then echo "$label: a chunk failed"; exit 1; fi
  local mem=$(for k in 0 1 2 3 4 5 6 7; do tail -1 full2/ov-err-$k; done | awk '{ if ($2 > m) m = $2; s += $1 } END { printf "%.2f %.1f", s, m / 1048576 }')
  echo "$label wall $wall cpu-sum-of-chunk-walls ${mem% *} peak-GB ${mem#* }"
}
for r in 1 2 3; do
  run base $HOME/iets-incr-base/target/release/iets batch-noq X=1
  run off $HOME/iets-incr/target/release/iets batch-noq X=1
  run trace $HOME/iets-incr/target/release/iets batch-noq IETS_FINE_TRACE=
  run queries $HOME/iets-incr/target/release/iets batch IETS_FINE_TRACE=
  run graph $HOME/iets-incr/target/release/iets batch IETS_FINE_TRACE= IETS_FINE_DAG=full2/ov-dag-@K@
done
rm -rf full2/ov-dag-*
