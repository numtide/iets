# Consumers use four shared tools, three rotating dependencies and one individual input.
let
  mk = name: inputs: builtins.derivation {
    inherit name inputs;
    system = "x86_64-linux";
    builder = "bench-builder";
  };
  producers = builtins.genList (i: mk "producer-${toString i}" []) 16;
  shared = builtins.genList (i: (mk "tool-${toString i}" []).outPath) 4;
  consumer = i:
    let
      rotating = builtins.genList (j:
        let k = i + j; in (builtins.elemAt producers (k - 16 * builtins.div k 16)).outPath
      ) 3;
      leaf = mk "leaf-${toString i}" [];
    in mk "consumer-${toString i}" (shared ++ rotating ++ [ leaf.outPath ]);
  paths = map (d: builtins.unsafeDiscardStringContext d.drvPath) (builtins.genList consumer 32);
in builtins.hashString "sha256" (builtins.concatStringsSep "\n" paths)
