# PosixSourceAccessor::readFile reads st_size bytes, so a /proc file that
# reports size 0 hashes as the empty string. Without procfs the case has
# nothing to test and returns the expected value.
let
  f = /proc/version;
  empty = algo: builtins.hashFile algo f == builtins.hashString algo "";
in
if builtins.pathExists f then [ (empty "sha256") (empty "sha1") ] else [ true true ]
