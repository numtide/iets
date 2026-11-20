let
  pos = builtins.unsafeGetAttrPos "x";
  source = { x = 1; };
  mapped = builtins.mapAttrs (_: v: v + 1) source;
  unforced = builtins.mapAttrs (throw "unused mapper") { x = throw "unused input"; };
  # One more entry than the packed map thunk can represent.
  wide = source // builtins.listToAttrs (
    builtins.genList (i: { name = toString i; value = null; }) 65536
  );
in
assert pos source != null;
assert pos (builtins.mapAttrs (throw "unused mapper") { }) == null;
assert pos unforced == null;
assert mapped.x == 2 && pos mapped == null;
assert pos (builtins.mapAttrs (throw "unused mapper") wide) == null;
true
