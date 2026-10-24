#!/usr/bin/env bash
# fsound.sh PAIRS: per step, the jobs of full/jobs whose drvPath changed (or became ERR, or went away)
# and that PAIRS ("job\tstep" lines) does not have: a re-evaluation the trace missed.
cd ~/incr
Z=$(nix build --no-link --print-out-paths nixpkgs#zstd.bin 2>/dev/null | head -1)/bin
cs=($(git -C nixpkgs log --first-parent --format=%h --reverse 9dbd131e6c65~1..4e63084a455b))
pairs() { for ((i = 1; i < ${#cs[@]}; i++)); do echo "$i ${cs[i-1]} ${cs[i]}"; done; echo "w100 ${cs[0]} ${cs[100]}"; for ((i = 10; i <= 100; i += 10)); do echo "w10-$i ${cs[i-10]} ${cs[i]}"; done; }
pairs | while read n a b; do
  gawk -F'\t' -v n=$n '
    FILENAME == ARGV[1] { if ($2 == n) inv[$1] = 1; next }
    FILENAME == ARGV[2] { job[$1] = 1; next }
    FILENAME == ARGV[3] { d[$1] = $2; next }
    { seen[$1] = 1; if (($1 in job) && d[$1] != $2) { c++; if (!($1 in inv)) { m++; if (m <= 3) ml = ml " " $1 } } }
    END {
      for (j in job) if (!(j in seen) && (j in d)) { c++; if (!(j in inv)) { m++; if (m <= 3) ml = ml " " j } }
      printf "%s\tchanged %d\tmissed %d%s\n", n, c, m, ml
    }' "$1" full/jobs <($Z/zstdcat drvs/$a.txt.zst) <($Z/zstdcat drvs/$b.txt.zst)
done
