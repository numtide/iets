let
  rev = "0123456789012345678901234567890123456789";
in
{
  parsed = map builtins.parseFlakeRef [
    "sourcehut:~a/b"
    "sourcehut:~a/b/${rev}"
    "sourcehut:~a/b/release/v1.0?dir=sub"
    "sourcehut:a/b?ref=x%2Fy&host=git.example.org&unknown=1"
    "sourcehut:~a/b?rev=${rev}&narHash=sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
  ];
  shown = map builtins.flakeRefToString [
    {
      type = "sourcehut";
      owner = "~a";
      repo = "b";
    }
    {
      type = "sourcehut";
      owner = "~a";
      repo = "b";
      ref = "x/y";
    }
    {
      type = "sourcehut";
      owner = "~a";
      repo = "b";
      host = "git.example.org";
      inherit rev;
    }
  ];
}
