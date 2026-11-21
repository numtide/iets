# The nix-eval-jobs functional-test jobset (tests-functional/assets/ci.nix),
# plus meta and a few shapes the walk must handle.
{
  system ? "x86_64-linux",
}:

let
  dep-a = derivation {
    name = "dep-a";
    inherit system;
    builder = "/bin/sh";
    args = [ "-c" "echo 'bbbbbb' > $out" ];
  };

  dep-b = derivation {
    name = "dep-b";
    inherit system;
    builder = "/bin/sh";
    args = [ "-c" "echo 'aaaaaa' > $out" ];
  };
in
{
  builtJob = derivation {
    name = "job1";
    inherit system;
    builder = "/bin/sh";
    args = [ "-c" "echo 'job1' > $out" ];
    requiredSystemFeatures = [ "big-parallel" ];
  };

  dontRecurse = {
    drvB = derivation {
      inherit system;
      name = "drvA";
      builder = ":";
    };
  };

  "dotted.attr" = derivation {
    name = "dotted";
    inherit system;
    builder = "/bin/sh";
    args = [ "-c" "echo 'dotted' > $out" ];
  };

  # names that are not plain identifiers, which `attr` quotes (whole
  # nixpkgs has `vscode-extensions."1Password"` and `nerd-fonts."m+"`)
  quoted = {
    recurseForDerivations = true;
    "1Password" = derivation {
      name = "digit-first";
      inherit system;
      builder = ":";
    };
    "m+" = derivation {
      name = "plus";
      inherit system;
      builder = ":";
    };
    "q\"u\\o$te" = derivation {
      name = "escaped";
      inherit system;
      builder = ":";
    };
  };

  package-with-deps = derivation {
    name = "package-with-deps";
    inherit system;
    builder = "/bin/sh";
    args = [ "-c" "echo '${dep-a} ${dep-b}' > $out" ];
  };

  recurse = {
    recurseForDerivations = true;
    drvB = derivation {
      inherit system;
      name = "drvB";
      builder = ":";
    };
  };

  withMeta = (derivation {
    name = "with-meta";
    inherit system;
    builder = "/bin/sh";
    args = [ "-c" "echo meta > $out" ];
    outputs = [ "out" "dev" ];
  }) // {
    meta = {
      description = "a job with meta";
      license = { shortName = "mit"; free = true; };
      platforms = [ "x86_64-linux" "aarch64-linux" ];
      priority = 5;
      score = 1.5;
      broken = false;
      nullDropped = null;
      fnDropped = x: x;
    };
  };

  broken = throw "this job does not evaluate";

  notAJob = "a string";

  aFunction = { x ? 1 }: derivation {
    name = "auto-called";
    inherit system;
    builder = ":";
  };
}
