let
  check = forceFirst: f:
    let
      a = [ (builtins.seq 1 f) ];
      b = [ (builtins.seq 2 f) ];
      shared = builtins.seq 1 f;
      mapped = x: builtins.mapAttrs (_: _: f) { inherit x; };
      compare = x: y: if forceFirst then builtins.deepSeq x (builtins.deepSeq y (x == y)) else x == y;
    in [
      (compare a b)
      (compare [ shared ] [ shared ])
      (compare (mapped 1) (mapped 2))
      (compare [ f ] [ f ])
      # Comparing once already forces both list elements.
      (if a == b then false else a != b)
    ];
  run = forceFirst: map (check forceFirst) [ (x: x) builtins.add (builtins.add 1) ];
in
map run [ false true ]
