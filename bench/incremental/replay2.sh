#!/usr/bin/env bash
# replay2.sh FIRST LAST: replay.sh with query cutoffs. A resident checker (fine-check --serve) keeps
# the live trace graphs. Per step, round 1 evaluates the hard and new jobs and the fingerprint of
# every suspect query, round 2 the suspect jobs whose queries changed. Every drvPath, kept or new,
# is compared with the full evaluation of that commit (drvs/). NOCUT=1 evaluates every suspect, for
# comparison; R names the work directory.
set -u
cd ~/incr
I=$HOME/iets-incr/target/release/iets
B=$HOME/incr/base
Z=$(nix build --no-link --print-out-paths nixpkgs#zstd.bin 2>/dev/null | head -1)/bin
cs=($(git -C nixpkgs log --first-parent --format=%h --reverse 9dbd131e6c65~1..4e63084a455b))
R=${R:-replay2}
. ./batch-head.sh
mkdir -p $R
now() { date +%s.%N; }
since() { awk -v a=$1 -v b=$(now) 'BEGIN { printf "%.2f", b - a }'; }
if [ ! -s $R/owners ]; then
  # job, graph, index in the graph, drvPath; graph, step it was written at, kept for its queries
  paste <(cut -f3 full/index) <(cut -f1,2 full/index | sed 's|^full/|full2/|') <(cut -f2 full2/res.tsv) > $R/owners
  for k in 0 1 2 3 4 5 6 7; do printf "full2/dag-%d\t0\t0\n" $k; done > $R/graphs
fi
exec 9> ~/bld1.lock
flock 9
coproc SRV { exec $I fine-check --serve 2> $R/server.err; }
srv() {
  echo "$1" >&"${SRV[1]}"
  local r
  read -r r <&"${SRV[0]}"
  [ "$r" = ok ] || { echo "server: $1: $r" >&2; exit 1; }
}
while IFS=$'\t' read -r d s _; do srv "load $d $s"; done < $R/graphs

# batch DIR JOBS QUERIES: evaluates the jobs and the fingerprints of the queries at $B into graph DIR
batch() {
  local d=$1
  { head_of $B
    gawk -f qpaths.awk "$2"
    gawk -F'\t' '{ s = ""; for (i = 2; i <= NF; i++) { x = $i; gsub(/\\/, "\\\\", x); gsub(/"/, "\\\"", x); gsub(/\$/, "\\$", x); s = s " \"" x "\"" } print "  (q [" s " ])" }' "$3"
    echo "]"; } > $d.nix
  rm -rf $d
  IETS_FINE_TRACE= IETS_FINE_DAG=$d $I eval --strict --impure --no-ir-cache --no-tracing-cache --workers 1 \
    -E "import $HOME/incr/$d.nix" > $d.out 2> $d.err
  tr -d '[]' < $d.out | tr ' ' '\n' | grep -v '^$' | tr -d '"' > $d.vals
}

# round N JOBS QUERIES: evaluates them in up to 8 graphs; appends job, graph, index, drvPath to
# $R/new-owners and key, fingerprint to $R/fps
round() {
  local n=$1 nj nq k c pids=()
  nj=$(wc -l < $2) nq=$(wc -l < $3)
  [ $((nj + nq)) -eq 0 ] && return
  k=$(( (nj + nq + 1999) / 2000 )); [ $k -gt 8 ] && k=8
  for ((c = 0; c < k; c++)); do
    gawk -v c=$c -v k=$k 'NR % k == c' $2 > $R/j-$n-$c
    gawk -v c=$c -v k=$k 'NR % k == c' $3 > $R/q-$n-$c
    batch $R/dag-$step-$n-$c $R/j-$n-$c $R/q-$n-$c &
    pids+=($!)
  done
  # not a bare `wait`: the checker is a child too
  wait "${pids[@]}"
  for ((c = 0; c < k; c++)); do
    d=$R/dag-$step-$n-$c
    nj=$(wc -l < $R/j-$n-$c)
    head -n $nj $d.vals | paste $R/j-$n-$c - | gawk -F'\t' -v d=$d '{ print $1 "\t" d "\t" NR - 1 "\t" $2 }' >> $R/new-owners
    tail -n +$((nj + 1)) $d.vals | paste <(cut -f1 $R/q-$n-$c) - >> $R/fps
    srv "load $d $step"
    printf "%s\t%s\t%d\n" $d $step $([ -s $R/q-$n-$c ] && echo 1 || echo 0) >> $R/graphs
  done
}

printf "step\thard\tsuspect\tsolo\tsus_kept\tnew\tgone\tredo1\tqueries\tredo2\tcheck_s\teval1_s\teval2_s\twrong_kept\twrong_redo\n"
for ((step = $1; step <= $2; step++)); do
  a=${cs[step-1]} b=${cs[step]}
  [ "$(git -C $B rev-parse --short=12 HEAD)" = "$a" ] || { echo "worktree is not at $a"; exit 1; }
  until awk '{ exit !($1 < 1.0 && $2 < 1.5) }' /proc/loadavg; do sleep 10; done
  sed "s/^/$step\t/" fchg/$step.txt > $R/step.chg
  t=$(now)
  srv "check $R/step.chg $R/chk"
  check=$(since $t)
  $Z/zstdcat drvs/$b.txt.zst | gawk -F'\t' '$2 != "ERR"' > $R/want
  : > $R/redo1; : > $R/sus; : > $R/kept; : > $R/wrong; : > $R/queries; : > $R/new-owners; : > $R/fps
  gawk -F'\t' -v R=$R -v nocut=${NOCUT:-0} '
    FILENAME == ARGV[1] {
      if ($1 == "H") hard[$2 "\t" $3] = 1
      else if ($1 == "S") sus[$2 "\t" $3] = $4
      else if ($1 == "Q") { fp[$2] = $3; s = $2; for (i = 4; i <= NF; i++) s = s "\t" $i; names[$2] = s }
      next }
    FILENAME == ARGV[2] { want[$1] = $2; next }
    { own[$1] = 1; k = $2 "\t" $3
      if (k in hard) { nh++; if ($1 in want) print $1 > (R "/redo1"); else gone++ }
      else if (!($1 in want)) { gone++; wk++; print $1 "\tgone" > (R "/wrong") }
      else if (k in sus) { ns++; line[ns] = $0; keys[ns] = sus[k]; n = split(sus[k], ks, " "); for (i = 1; i <= n; i++) uses[ks[i]]++ }
      else print > (R "/kept") }
    END {
      for (j in want) if (!(j in own)) { nn++; print j > (R "/redo1") }
      # a suspect whose queries no other suspect reads costs as much to verify as to evaluate
      for (x = 1; x <= ns; x++) {
        n = split(keys[x], ks, " "); shared = 0
        for (i = 1; i <= n; i++) if (uses[ks[i]] > 1) shared = 1
        if (!shared || nocut) { solo++; split(line[x], f, "\t"); print f[1] > (R "/redo1"); continue }
        s = ""; for (i = 1; i <= n; i++) { s = s " " ks[i] "=" fp[ks[i]]; ask[ks[i]] = 1 }
        print line[x] "\t" substr(s, 2) > (R "/sus")
      }
      for (q in ask) print names[q] > (R "/queries")
      printf "%d\t%d\t%d\t%d\t%d\t%d\n", nh, ns, solo, nn, gone, wk > (R "/counts")
    }' $R/chk $R/want $R/owners
  git -C $B checkout -q --detach $b
  t=$(now)
  round 1 $R/redo1 $R/queries
  ev1=$(since $t)
  # a suspect is kept when every query it read kept its fingerprint
  gawk -F'\t' -v R=$R '
    FILENAME == ARGV[1] { now[$1] = $2; next }
    { n = split($5, ks, " "); bad = 0
      for (i = 1; i <= n; i++) { split(ks[i], kv, "="); if (!(kv[1] in now) || now[kv[1]] != kv[2]) bad = 1 }
      if (bad) print $1 > (R "/redo2"); else { kept++; print $1 "\t" $2 "\t" $3 "\t" $4 >> (R "/kept") } }
    END { print kept + 0 > (R "/sus-kept") }' $R/fps $R/sus
  touch $R/redo2
  t=$(now)
  : > $R/none
  round 2 $R/redo2 $R/none
  ev2=$(since $t)
  wk2=$(gawk -F'\t' 'NR == FNR { want[$1] = $2; next } want[$1] != $4 { w++ } END { print w + 0 }' $R/want $R/kept)
  wr=$(gawk -F'\t' 'NR == FNR { want[$1] = $2; next } want[$1] != $4 { w++; print > "/dev/stderr" } END { print w + 0 }' $R/want $R/new-owners 2> $R/wrong-redo-$step)
  [ -s $R/wrong-redo-$step ] || rm -f $R/wrong-redo-$step
  gawk -F'\t' 'NR == FNR { want[$1] = $2; next } want[$1] != $4' $R/want $R/kept > $R/wrong-kept-$step
  [ -s $R/wrong-kept-$step ] || rm -f $R/wrong-kept-$step
  cat $R/kept $R/new-owners > $R/owners.next && mv $R/owners.next $R/owners
  read nh ns solo nn gone wk < <(tr '\t' ' ' < $R/counts)
  printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" $step ${nh:-0} ${ns:-0} ${solo:-0} $(cat $R/sus-kept) ${nn:-0} ${gone:-0} \
    $(wc -l < $R/redo1) $(wc -l < $R/queries) $(wc -l < $R/redo2) $check $ev1 $ev2 $((${wk:-0} + wk2)) $wr
  if [ -s $R/queries ]; then
    gawk -F'\t' 'FILENAME == ARGV[1] { now[$1] = $2; next } { print $0 "\t" now[$1] }' $R/fps $R/queries > $R/queries-$step
  fi
  rm -f $R/redo2
  # graphs no job uses, whose traces the checker does not need either
  cut -f2 $R/owners | sort -u > $R/used
  gawk -F'\t' 'FILENAME == ARGV[1] { u[$1] = 1; next } !($1 in u) { print $1 }' $R/used $R/graphs > $R/unused
  srv "gc $R/unused $R/gone"
  while read -r d; do [[ $d == $R/* ]] && rm -rf $d $d.nix $d.out $d.err $d.vals; done < $R/gone
  gawk -F'\t' 'FILENAME == ARGV[1] { x[$1] = 1; next } !($1 in x)' $R/gone $R/graphs > $R/graphs.next && mv $R/graphs.next $R/graphs
done
