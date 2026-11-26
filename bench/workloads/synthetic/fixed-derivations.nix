let
  paths = builtins.genList (
    n:
    let
      i = n - (builtins.div n 100) * 100;
      d = builtins.derivation {
        name = "fixed-bench-${toString i}";
        system = "x86_64-linux";
        builder = "bench-builder";
        outputHashAlgo = "sha256";
        outputHash = builtins.hashString "sha256" (toString i);
        outputHashMode = if builtins.bitAnd i 1 == 0 then "flat" else "recursive";
      };
    in
    builtins.unsafeDiscardStringContext "${d.drvPath} ${d.outPath}"
  ) 500;
in
builtins.hashString "sha256" (builtins.concatStringsSep "\n" paths)
