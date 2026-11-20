# Updates with disjoint keys, overrides and lookups through the resulting set.
let
  names = builtins.genList (n: "key-${toString n}") 2000;
  base = builtins.listToAttrs (map (name: { inherit name; value = 1; }) names);
  updated = base // { key-0 = 2; key-1999 = 3; };
  result = builtins.foldl' (sum: name: sum + updated.${name}) 0 names;
in
assert result == 2003;
result
