let
  r = rec {
    inherit (x) y z;
    x = { y = 1; z = 1; };
    q = x.y;
    __overrides = { x = { y = 2; z = 3; }; };
  };
in
[ r.y r.z r.q r.x.y ]
