#!/usr/bin/env bash
# fchg/<n>.txt: what step n changed, in the terms of the fine trace (fcheck.awk reads it)
set -eu -o pipefail
export LC_ALL=C
cd ~/incr/nixpkgs
P=$HOME/incr/base
I=$HOME/iets-incr/target/release/iets
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
mkdir -p ../fchg

# Same byte encoding as fine-regions. Directory entry names cannot contain '/'.
field() {
  if [[ $1 =~ ^[a-zA-Z0-9._+/-]+$ ]]; then
    encoded=$1
  else
    encoded=~$(printf %s "$1" | od -An -v -tx1 | tr -d ' \n')
  fi
}
path_change() {
  field "$2"; printf '%s\t%s\n' "$1" "$encoded"
}
dir_change() {
  field "$1"; local dir=$encoded
  field "$2"; printf 'CD\t%s\t%s\n' "$dir" "$encoded"
}

cs=($(git log --first-parent --format=%h --reverse 9dbd131e6c65~1..4e63084a455b))
pairs() { for ((i = 1; i < ${#cs[@]}; i++)); do echo "$i ${cs[i-1]} ${cs[i]}"; done; echo "w100 ${cs[0]} ${cs[100]}"; for ((i = 10; i <= 100; i += 10)); do echo "w10-$i ${cs[i-10]} ${cs[i]}"; done; }
pairs | while read -r n a b; do
  out=../fchg/$n.txt
  if [[ -s $out ]] && grep -Fqx $'V\t2' "$out"; then continue; fi
  {
    printf 'V\t2\n'
    git diff --no-renames --name-status -z "$a" "$b" | while IFS= read -r -d '' st && IFS= read -r -d '' p; do
      path_change CM "$P/$p"
      q=$p
      while :; do path_change CT "$P/$q"; [[ $q == */* ]] || break; q=${q%/*}; done
      path_change CT "$P"
      case $st in
      A | D)
        [[ $st == A ]] && other=$a || other=$b
        pre="" rest="$p/" missing=false
        while [[ -n $rest ]]; do
          name=${rest%%/*} rest=${rest#*/}
          cand=${pre:+$pre/}$name
          if ! $missing && ! git cat-file -e "$other:$cand" 2>/dev/null; then
            dir_change "$P${pre:+/$pre}" "$name"
            missing=true
          fi
          if $missing; then path_change CS "$P/$cand"; fi
          pre=$cand
        done
        ;;
      T)
        dir=$P
        if [[ $p == */* ]]; then dir=$P/${p%/*}; fi
        dir_change "$dir" "${p##*/}"
        path_change CS "$P/$p"
        ;;
      esac
      if [[ $p == *.nix && $st != A ]]; then
        git show "$a:$p" > "$T/a.nix"
        "$I" fine-regions "$T/a.nix=$P/$p" > "$T/a.tab"
        if [[ $st == D ]]; then
          printf 'V\t2\nE\n' > "$T/b.tab"
        else
          git show "$b:$p" > "$T/b.nix"
          "$I" fine-regions "$T/b.nix=$P/$p" > "$T/b.tab"
        fi
        field "$P/$p"
        gawk -f ../fcmp.awk "$T/a.tab" "$T/b.tab"
        if [[ $st == D ]]; then d=1; else d=$( { cmp "$T/a.nix" "$T/b.nix" 2>/dev/null || :; } | gawk '{print $5 + 0}'); fi
        if [[ -n $d ]]; then printf 'CX\t%s\t%d\n' "$encoded" "$((d - 1))"; fi
      fi
    done
  } | sort -u > "$out.tmp"
  mv "$out.tmp" "$out"
done
