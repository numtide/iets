# Whole nixpkgs as one strict evaluation: every job of nixpkgs' own CI job
# set (ci/eval/outpaths.nix) as `{ "attr.path" = drvPath or null; }`, null
# where the job throws. `eval --strict --json` forces all of it, the work
# `eval-jobs` spreads over its walk, in one value.
#
#   iets eval --impure --strict --json --arg nixpkgs /path/to/nixpkgs bench/workloads/outpaths-strict.nix
{ nixpkgs }:
let
  lib = import (nixpkgs + "/lib");
  # the native system only: nixpkgs names some jobs by the system it runs on
  jobs = import (nixpkgs + "/ci/eval/outpaths.nix") {
    path = nixpkgs;
    systems = [ builtins.currentSystem ];
  };
  try = v: builtins.tryEval v;
  isDrv = v: ((try (v.type or null)).value or null) == "derivation";
  walk =
    prefix: value:
    let
      r = try value;
    in
    if !r.success then
      [ { name = prefix; value = null; } ]
    else if isDrv r.value then
      [ { name = prefix; value = let d = try r.value.drvPath; in if d.success then d.value else null; } ]
    # the root is not marked recurseForDerivations, the nested sets are
    else if
      builtins.isAttrs r.value
      && (prefix == "" || ((try (r.value.recurseForDerivations or false)).value or false))
    then
      lib.concatLists (
        lib.mapAttrsToList (n: v: walk (if prefix == "" then n else "${prefix}.${n}") v) (
          removeAttrs r.value [ "recurseForDerivations" ]
        )
      )
    else
      [ ];
in
builtins.listToAttrs (walk "" jobs)
