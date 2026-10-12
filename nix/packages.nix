{ lib, stdenv, iets, ietsStatic, classic, fuzz, coverage }:
{
  default = iets;
  # .drv files read by harmonia-store-aterm (Cargo.toml)
  harmonia-aterm = iets.override { extraArgs.cargoExtraArgs = "--locked --features harmonia-aterm"; };
  inherit classic;
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
// lib.optionalAttrs (stdenv.hostPlatform.system == "x86_64-linux") {
  # line coverage of the e2e tests in NixOS containers (nix/coverage.nix)
  coverage-e2e = coverage;
}
