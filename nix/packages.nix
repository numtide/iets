{ iets, ietsStatic, ietsWasi, classic, fuzz }:
{
  default = iets;
  inherit classic;
  # one musl file that needs nothing on the machine (docs/explanation/static-build.md)
  static = ietsStatic;
  # `bin/iets.wasm` for a browser (docs/explanation/webassembly.md)
  wasi = ietsWasi;
  fuzz-oracles = fuzz.oracles;
  inherit (fuzz) fuzz-diff fuzzers;
  fuzz-parse-campaign = fuzz.campaign { target = "parse"; };
  fuzz-nar-campaign = fuzz.campaign { target = "nar-gen"; };
  fuzz-wire-campaign = fuzz.campaign { target = "wire"; };
  fuzz-eval-campaign = fuzz.campaign { target = "eval"; };
  fuzz-drv-campaign = fuzz.campaign { target = "drv-gen"; };
  fuzz-drv-eval-campaign = fuzz.campaign { target = "drv-eval"; };
}
