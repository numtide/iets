# The full string and its eight-byte prefix have the same content hash.
let
  s = "00110400/NlM8Qm'";
  cut = n: builtins.substring 0 n s;
in builtins.seq (cut 8) (cut 16)
