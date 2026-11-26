# Repeated calls and list forcing. The assertion makes skipped work a failure.
let
  fib = n: if n < 2 then n else fib (n - 1) + fib (n - 2);
  result = builtins.foldl' (a: b: a + b) 0 (builtins.genList (_: fib 16) 16);
in
assert result == 15792;
result
