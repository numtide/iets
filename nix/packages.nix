{ iets, ietsStatic, classic, fuzz, benchmarks }:
{
  default = iets;
  inherit classic;
  bench-ab = benchmarks.ab;
  bench-callgrind = benchmarks.callgrind;
  bench-workloads = benchmarks.workloads;
  # one musl file that needs nothing on the machine (docs/explanation/static-build.md)
  static = ietsStatic;
  fuzz-oracles = fuzz.oracles;
  inherit (fuzz) fuzz-diff fuzzers;
  fuzz-parse-campaign = fuzz.campaign { target = "parse"; };
  fuzz-nar-campaign = fuzz.campaign { target = "nar-gen"; };
  fuzz-wire-campaign = fuzz.campaign { target = "wire"; };
  fuzz-eval-campaign = fuzz.campaign { target = "eval"; };
  fuzz-drv-campaign = fuzz.campaign { target = "drv-gen"; };
  fuzz-drv-eval-campaign = fuzz.campaign { target = "drv-eval"; };
}
