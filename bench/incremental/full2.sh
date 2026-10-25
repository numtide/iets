#!/usr/bin/env bash
# full2.sh plain|traced: every job that has a drvPath at base, as `pkgs.<path>.drvPath` of a package set the
# batch imports itself (batch-head.sh), in 8 chunks of one worker each, at once. traced writes the trace
# graph of chunk k, with its queries, to full2/dag-k.
set -u
cd ~/incr
mode=$1
I=$HOME/iets-incr/target/release/iets
B=$HOME/incr/base
Z=$(nix build --no-link --print-out-paths nixpkgs#zstd.bin 2>/dev/null | head -1)/bin
mkdir -p full full2
if [ ! -s full/jobs ]; then
  $Z/zstdcat drvs/$(git -C base rev-parse --short=12 HEAD).txt.zst | gawk -F'\t' '$2 != "ERR" { print $1 }' > full/jobs
fi
. ./batch-head.sh
for k in 0 1 2 3 4 5 6 7; do
  gawk -v k=$k 'NR % 8 == k' full/jobs > full/chunk-$k
  { head_of $B
    gawk -f qpaths.awk full/chunk-$k
    echo "]"; } > full2/batch-$k.nix
done
exec 9> ~/bld1.lock
flock 9
until awk '{ exit !($1 < 1.0 && $2 < 1.5) }' /proc/loadavg; do sleep 10; done
uptime
t0=$(date +%s.%N)
. ./batch-head.sh
for k in 0 1 2 3 4 5 6 7; do
  (
    if [ $mode = traced ]; then
      rm -rf full2/dag-$k
      export IETS_FINE_TRACE= IETS_FINE_DAG=full2/dag-$k
    fi
    /usr/bin/env time -f "chunk $k: %e s %M KB" $I eval --strict --impure --no-ir-cache --no-tracing-cache --workers 1 \
      -E "import $HOME/incr/full2/batch-$k.nix" > full2/out-$mode-$k 2> full2/err-$mode-$k
    tail -1 full2/err-$mode-$k
  ) &
done
wait
echo "$mode wall: $(awk -v a=$t0 -v b=$(date +%s.%N) 'BEGIN { print b - a }') s"
uptime
