# Hydra aggregates for --constituents, from nix-eval-jobs's flake fixture:
# every case in one jobset, one attrset per case.
{
  system ? "x86_64-linux",
}:
let
  mk = name: derivation {
    inherit name system;
    builder = "/bin/sh";
    args = [ "-c" "echo done > $out" ];
  };
  aggregate = name: constituents: (mk name) // { _hydraAggregate = true; inherit constituents; };
  globAggregate = name: constituents: (aggregate name constituents) // { _hydraGlobConstituents = true; };
  makeTextDrv = name: text: derivation {
    inherit name system;
    builder = "/bin/sh";
    args = [ "-c" "echo '${text}' > $out" ];
  };
in
{
  success = {
    recurseForDerivations = true;
    anotherone = makeTextDrv "constituent" "text";
    builtJob = mk "job1";
    "1Password" = mk "digit-first";
    quoted = aggregate "quoted" [ ''success."1Password"'' ];
    unquoted = aggregate "unquoted" [ "success.1Password" ];
    quotedGlob = globAggregate "quoted-glob" [ ''success."1Pass*"'' ];
    indirect_aggregate = aggregate "indirect_aggregate" [ "success.anotherone" ];
    direct_aggregate = aggregate "direct_aggregate" [ (mk "job1") ];
    mixed_aggregate = aggregate "mixed_aggregate" [ (mk "job1") "success.anotherone" ];
    nested_aggregate = aggregate "nested_aggregate" [ "success.indirect_aggregate" "success.mixed_aggregate" ];
  };
  failures = {
    recurseForDerivations = true;
    aggregate = aggregate "aggregate" [ "failures.doesntexist" "failures.doesnteval" ];
    doesnteval = makeTextDrv "constituent" (toString { });
    missingOnly = aggregate "missing-only" [ "nowhere.at.all" ];
  };
  glob1 = {
    recurseForDerivations = true;
    constituentA = mk "constituentA";
    constituentB = mk "constituentB";
    aggregate = globAggregate "aggregate" [ "glob1.*" ];
  };
  glob2 = rec {
    recurseForDerivations = true;
    packages = {
      recurseForDerivations = true;
      constituentA = mk "constituentA";
      constituentB = mk "constituentB";
    };
    aggregate0 = globAggregate "aggregate0" [ "glob2.packages.*" ];
    aggregate1 = globAggregate "aggregate1" [ "glob2.tests.*" ];
    indirect_aggregate0 = aggregate "indirect_aggregate0" [ "glob2.aggregate0" ];
    mix_aggregate0 = aggregate "mix_aggregate0" [ "glob2.aggregate0" packages.constituentA ];
  };
  cycle = {
    recurseForDerivations = true;
    aggregate0 = globAggregate "aggregate0" [ "cycle.aggregate1" ];
    aggregate1 = globAggregate "aggregate1" [ "cycle.aggregate0" ];
    bystander = aggregate "bystander" [ "cycle.aggregate0" ];
  };
  noConstituents = (mk "no-constituents") // { _hydraAggregate = true; };
  notAggregate = (mk "not-aggregate") // { _hydraAggregate = false; constituents = [ "success.builtJob" ]; };
}
