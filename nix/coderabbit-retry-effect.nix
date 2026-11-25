# Hourly `@coderabbitai review` on rate-limited pull requests. CodeRabbit
# ignores bots, so the token is a user's (org: machines/bld1/nixbot.nix).
{ pkgs, hci-effects, repo }:
hci-effects.mkEffect {
  name = "coderabbit-retry";
  inputs = [ pkgs.gh pkgs.python3 ];
  secretsMap.coderabbit = "coderabbit";
  effectScript = ''
    export GH_TOKEN=$(readSecretString coderabbit .token)
    python3 ${./coderabbit_retry.py} --repo ${repo}
  '';
}
