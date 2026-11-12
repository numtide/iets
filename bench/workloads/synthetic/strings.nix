# Unique strings, substring sharing, concatenation and a strict fold.
let
  result = builtins.foldl' (sum: n:
    sum + builtins.stringLength (builtins.substring 1 5 ("prefix-${toString n}-suffix"))
  ) 0 (builtins.genList (n: n) 10000);
in
assert result == 50000;
result
