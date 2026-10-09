# Hashes from a lock file, so a fetcher call names a URL and no hash.
# Plain Nix: it evaluates the same on nix. `iets lock` is the only writer
# (docs/explanation/locked-hashes.md).
#
#   let lock = import ./nix/lock.nix ./iets.lock; in
#   lock.fetchTarball "https://example.org/src.tar.gz"
#   pkgs.fetchurl { inherit url; hash = lock.hash { inherit url; }; }
file:
let
  name = baseNameOf (toString file);
  doc =
    if builtins.pathExists file then builtins.fromJSON (builtins.readFile file) else { version = 1; };
  # keep in step with `command` in src/cli/lock.rs
  command =
    unpack: update: url:
    builtins.concatStringsSep " " (
      [ "iets lock" ]
      ++ (if name == "iets.lock" then [ ] else [ "--file ${name}" ])
      ++ (if unpack then [ "--unpack" ] else [ ])
      ++ (if update then [ "--update" ] else [ ])
      ++ [ url ]
    );
  hash =
    {
      url,
      unpack ? false,
    }:
    let
      mode = if unpack then "unpack" else "file";
    in
    if doc.version or null != 1 then
      throw "${name}: version ${
        toString (doc.version or "(missing)")
      } is not supported; this helper reads version 1"
    else
      doc.${mode}.${url} or (throw ''
        ${name} has no ${mode} hash for ${url}.
        Check that you trust the URL, then run this in the directory of ${name}:
          ${command unpack false url}'');
in
{
  inherit hash;
  fetchTarball =
    args:
    let
      a = if builtins.isString args then { url = args; } else args;
      sha256 = hash {
        inherit (a) url;
        unpack = true;
      };
    in
    builtins.seq sha256 (
      builtins.addErrorContext
        "while fetching ${a.url} with the hash in ${name}; on a hash mismatch, the remote changed since it was locked: check it, then run '${command true true a.url}'"
        (builtins.fetchTarball (a // { inherit sha256; }))
    );
}
