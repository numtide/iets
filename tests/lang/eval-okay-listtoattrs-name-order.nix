(builtins.tryEval (builtins.listToAttrs [
  { name = "first"; }
  { name = throw "later name"; }
])).success
