# The e2e checks: scripts/NAME.nu in a NixOS test container (nix/e2e-test.nix),
# with what each one needs. flake.nix runs them as checks, nix/coverage.nix
# runs them with an instrumented iets.
{ pkgs }:
let
  nix-grpc-daemon = import ./nix-grpc-store.nix { inherit pkgs; };
  # What an interrupted copy leaves: files at the store paths classic-e2e's
  # fetchurl and fetchTarball cases name, which the store never registered.
  leftovers = pkgs.writeShellScript "leftovers" ''
    set -eu
    f=$(nix-store --print-fixed-path sha256 113a52l57f2jv9fh9igj2xh2mi0is40flxbkqi69wbh7nlybkcbm leftover)
    z=$(nix-store --print-fixed-path sha256 0000000000000000000000000000000000000000000000000000 leftover)
    echo stale > "$f"
    echo stale > "$z"
    d=$(mktemp -d)
    echo hi > "$d/f"
    t=$(nix-store --print-fixed-path --recursive sha256 "$(nix-hash --type sha256 --base32 "$d")" leftover-tree)
    mkdir "$t"
    echo stale > "$t/f"
  '';
in
{
  cachetest = { };
  classic-e2e = { setup = "${leftovers}"; };
  daemon-e2e = { packages = [ pkgs.jq ]; };
  evaljobs-flags = {
    packages = [ pkgs.jq pkgs.nix-eval-jobs ];
    env = "NEJ=${pkgs.nix-eval-jobs}/bin/nix-eval-jobs";
  };
  build-e2e = { };
  fetchclosure-e2e = { };
  flaketest = { };
  grpc-e2e = {
    packages = [ nix-grpc-daemon ];
    env = "DAEMON=${nix-grpc-daemon}";
  };
  plumbing-e2e = { };
  prefetch-e2e = { };
}
