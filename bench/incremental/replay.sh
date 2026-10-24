#!/usr/bin/env bash
# replay.sh FIRST LAST: incremental evaluation of the first-parent merges FIRST..LAST of the window.
# Each step checks the live trace graphs against the step's changes, moves the worktree `base` to
# the new commit, evaluates again only the jobs whose trace a change reached and the new jobs, and
# compares every drvPath, kept or new, with the full evaluation of that commit (drvs/).
# Shortcut: the job list of a commit comes from drvs/ (no enumeration), and its error jobs are left out.
set -u
cd ~/incr
I=$HOME/iets-incr/target/release/iets
B=$HOME/incr/base
Z=$(nix build --no-link --print-out-paths nixpkgs#zstd.bin 2>/dev/null | head -1)/bin
cs=($(git -C nixpkgs log --first-parent --format=%h --reverse 9dbd131e6c65~1..4e63084a455b))
R=replay
mkdir -p $R
now() { date +%s.%N; }
since() { awk -v a=$1 -v b=$(now) 'BEGIN { printf "%.2f", b - a }'; }
if [ ! -s $R/owners ]; then
  # job, graph, index in the graph, drvPath
  paste <(cut -f3 full/index) <(cut -f1,2 full/index) <(cut -f2 full/res-traced.tsv) > $R/owners
fi
exec 9> ~/bld1.lock
flock 9
printf "step\tinvalid\tnew\tgone\tredo\tcheck_s\teval_s\twrong_kept\twrong_redo\n"
for ((n = $1; n <= $2; n++)); do
  a=${cs[n-1]} b=${cs[n]}
  [ "$(git -C $B rev-parse --short=12 HEAD)" = "$a" ] || { echo "worktree is not at $a"; exit 1; }
  until awk '{ exit !($1 < 1.0 && $2 < 1.5) }' /proc/loadavg; do sleep 10; done
  sed "s/^/$n\t/" fchg/$n.txt > $R/step.chg
  t=$(now)
  $I fine-check $R/step.chg $(cut -f2 $R/owners | sort -u) > $R/inv
  check=$(since $t)
  $Z/zstdcat drvs/$b.txt.zst | gawk -F'\t' '$2 != "ERR"' > $R/want
  # invalid owned jobs that still exist, and jobs not owned yet; the rest of the owned jobs are kept
  gawk -F'\t' -v R=$R '
    FILENAME == ARGV[1] { inv[$1 "\t" $2] = 1; next }
    FILENAME == ARGV[2] { want[$1] = $2; next }
    { own[$1] = 1
      if (($2 "\t" $3) in inv) { ninv++; if ($1 in want) print $1 > (R "/redo"); else gone++ }
      else if (!($1 in want)) { gone++; wk++; print $1 "\tgone" > (R "/wrong") }
      else { print > (R "/kept"); if (want[$1] != $4) { wk++; print $1 "\t" $4 "\t" want[$1] > (R "/wrong") } } }
    END {
      for (j in want) if (!(j in own)) { nnew++; print j > (R "/redo") }
      printf "%d\t%d\t%d\t%d\n", ninv, nnew, gone, wk > (R "/counts")
    }' $R/inv $R/want $R/owners
  touch $R/redo $R/kept $R/wrong
  git -C $B checkout -q --detach $b
  nredo=$(wc -l < $R/redo)
  t=$(now)
  wr=0
  : > $R/new-owners
  if [ $nredo -gt 0 ]; then
    k=$(( (nredo + 1999) / 2000 )); [ $k -gt 8 ] && k=8
    for ((c = 0; c < k; c++)); do
      gawk -v c=$c -v k=$k 'NR % k == c' $R/redo > $R/redo-$c
      { echo "let lib = import $B/lib; o = import $B/ci/eval/outpaths.nix { path = $B; systems = [ \"x86_64-linux\" ]; includeBroken = false; }; in map (p: (lib.attrByPath p null o).drvPath) ["
        gawk -f paths.awk $R/redo-$c
        echo "]"; } > $R/batch-$c.nix
      d=$R/dag-$n-$c
      rm -rf $d
      ( IETS_FINE_TRACE= IETS_FINE_DAG=$d $I eval --strict --impure --no-ir-cache --no-tracing-cache --workers 1 \
          -E "import $HOME/incr/$R/batch-$c.nix" > $R/out-$c 2> $R/err-$c ) &
    done
    wait
    for ((c = 0; c < k; c++)); do
      tr -d '[]' < $R/out-$c | tr ' ' '\n' | grep -v '^$' | tr -d '"' | paste $R/redo-$c - |
        gawk -F'\t' -v d=$R/dag-$n-$c '{ print $1 "\t" d "\t" NR - 1 "\t" $2 }' >> $R/new-owners
    done
    wr=$(gawk -F'\t' 'NR == FNR { want[$1] = $2; next } want[$1] != $4 { w++ } END { print w + 0 }' $R/want $R/new-owners)
  fi
  ev=$(since $t)
  cat $R/kept $R/new-owners > $R/owners.next && mv $R/owners.next $R/owners
  read ninv nnew gone wk < <(tr '\t' ' ' < $R/counts)
  printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" $n ${ninv:-0} ${nnew:-0} ${gone:-0} $nredo $check $ev ${wk:-0} $wr
  [ -s $R/wrong ] && cp $R/wrong $R/wrong-$n
  rm -f $R/redo $R/kept $R/wrong $R/counts
  # graphs no job uses any more
  for d in $R/dag-*; do [ -d $d ] && ! grep -qF "	$d	" $R/owners && rm -rf $d; done
done
