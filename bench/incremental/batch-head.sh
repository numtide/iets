# batch-head.sh: sourced; `head_of DIR` prints the start of a batch over the package set of
# ci/eval/outpaths.nix for x86_64-linux at the nixpkgs in DIR, with its cells named as queries.
# A batch is `head_of DIR`, the jobs (`(at [ ... ]).drvPath`) and queries (`q [ ... ]`), and `]`.
head_of() {
  cat <<EOF
let
  B = $1;
  lib = import (B + "/lib");
  pkgs = import B {
    system = "x86_64-linux";
    crossSystem = null;
    config = {
      allowAliases = false;
      allowBroken = false;
      allowUnfree = true;
      allowInsecurePredicate = x: true;
      allowVariants = true;
      checkMeta = true;
      problems.matchers = lib.mkForce [ { kind = "broken"; handler = "error"; } ];
      attrPathsDisallowedForInternalUse = [ ];
      allowDeprecatedx86_64Darwin = true;
      handleEvalIssue = reason: errormsg:
        if builtins.elem reason [ "unknown-meta" "broken-outputs" ] then abort errormsg
        else if builtins.elem reason [ "broken" "unfree" ] then throw "broken"
        else if builtins.elem reason [ "unsupported" ] then throw "unsupported"
        else true;
      inHydra = true;
    };
    __allowFileset = false;
  };
  at = p: lib.attrByPath p null pkgs;
  q = p: let r = builtins.tryEval (builtins.fineQuery (at p)); in if r.success then r.value else "-";
in
builtins.seq (builtins.fineRoot pkgs) [
EOF
}
