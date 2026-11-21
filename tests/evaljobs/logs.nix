# builtins.warn and builtins.trace while a job evaluates: nix-eval-jobs
# 2.35.4 puts them on its line as `warnings` and `traces`.
let
  drv =
    name:
    derivation {
      inherit name;
      system = "x86_64-linux";
      builder = ":";
    };
  esc = builtins.fromJSON ''"\u001b"'';
in
{
  traced = builtins.trace "a trace" (drv "traced");
  warned = builtins.warn "a warning" (drv "warned");
  both = builtins.warn "w1" (builtins.trace { a = 1; } (builtins.warn "w2" (drv "both")));
  colored = builtins.trace "${esc}[1;31mred${esc}[0m" (drv "colored");
  failing = builtins.trace "before the throw" (throw "boom");
  plain = drv "plain";
  # what prints while a set is evaluated goes on no line
  set = builtins.trace "while the set" {
    recurseForDerivations = true;
    inner = builtins.warn "inner" (drv "inner");
  };
}
