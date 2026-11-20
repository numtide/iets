# Distinct derivations plus repeated contents expose hashing/cache tradeoffs.
let
  drvs = builtins.genList (n: builtins.derivation {
    name = "bench-${toString (n - (builtins.div n 100) * 100)}";
    system = "x86_64-linux";
    builder = "bench-builder";
    args = [ "-c" "exit 0" ];
  }) 500;
  paths = map (d: builtins.unsafeDiscardStringContext d.drvPath) drvs;
  count = builtins.length (builtins.attrNames (builtins.listToAttrs
    (map (path: { name = path; value = true; }) paths)));
  digest = builtins.hashString "sha256" (builtins.concatStringsSep "\n" paths);
in
assert count == 100;
assert digest == "93992c8668eb390e01499c1ea64d40863403ba29ceb9ffb4b544a3c9976e79f9";
{ inherit count digest; }
