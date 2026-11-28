# These comparators are inconsistent (for example `a < b` and `b < a` can
# both be true), so there is no correct sorted result. The output is still
# deterministic, and iets must produce the same list as Nix does.
let
  mod = a: m: a - (a / m) * m;
  gen = n: c: builtins.genList (i: mod (i * c) (2 * n)) n;
  cases = [
    { n = 17; c = 7919; f = p: q: builtins.bitAnd (builtins.bitXor (p * 31) (q * 17)) 4 == 0; }
    { n = 33; c = 104729; f = p: q: mod (p * 5 + q * 3) 7 <= 2; }
    { n = 64; c = 15485863; f = p: q: mod p 3 < mod q 3; }
    { n = 100; c = 7919; f = p: q: p != q; }
    { n = 100; c = 104729; f = p: q: p >= q; }
    { n = 257; c = 15485863; f = p: q: builtins.bitAnd (p + q) 1 == 0; }
  ];
in
  map (x: builtins.sort x.f (gen x.n x.c)) cases
