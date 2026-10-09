# scripts/<name>.nu in a NixOS test container: a real nix-daemon, a writable
# store, the real nix on PATH and no network.
# With `coverage`, `iets` is an instrumented build: each process writes its
# profile, and $out/cov holds them all after the run.
{ self, nixpkgs, pkgs, iets, name, packages ? [ ], env ? "", coverage ? false }:
let
  inherit (pkgs) lib;
  system = pkgs.stdenv.hostPlatform.system;
  profile = lib.optionalString coverage "LLVM_PROFILE_FILE=/tmp/cov/%p-%m.profraw";
  prepare = lib.optionalString coverage ''machine.succeed("mkdir -m 1777 /tmp/cov")'';
  collect = lib.optionalString coverage ''machine.copy_from_machine("/tmp/cov", "cov")'';
  # nixpkgs/shell.nix imports this tree through a sha256-pinned fetchTarball;
  # the same store path, so the container's nix finds it and fetches nothing.
  pin = (pkgs.lib.importJSON "${nixpkgs}/ci/pinned.json").pins.nixpkgs;
  pinned = pkgs.fetchzip { inherit (pin) url hash; };
  # Every store path a case realises. Nothing is substituted, so what is not
  # here gets built, and only the scripts' own /bin/sh derivations can be.
  offline = [
    "${nixpkgs}"
    "${self}"
    pinned
    iets
    pkgs.hello
    pkgs.hello.inputDerivation
    pkgs.bashInteractive
    pkgs.coreutils
    pkgs.nix-output-monitor
    self.devShells.${system}.default.inputDerivation
  ];
  # The container gets the build's store as one read-only bind mount per
  # path, which no overlay can see through. So the writable store is a fresh
  # directory with the closure bound into it, path by path, and a database
  # that knows exactly those paths. The registration is opened first: the new
  # store hides it.
  writableStore = pkgs.writeShellScript "writable-store" ''
    PATH=${pkgs.lib.makeBinPath [ pkgs.jq pkgs.sqlite ]}:$PATH
    set -eu
    exec 3< "$1/registration"
    mkdir -p /nix/.store
    while read -r p; do
      t=/nix/.store/''${p##*/}
      if [ -d "$p" ]; then mkdir "$t"; else touch "$t"; fi
      mount --bind "$p" "$t"
    done < "$1/store-paths"
    mount --rbind /nix/.store /nix/store
    NIX_REMOTE=local nix-store --load-db <&3
    # closureInfo has no content addresses, and a real store has one for every
    # source nix added. A path with no references whose name and NAR hash give
    # its own store path is such a source.
    NIX_REMOTE=local nix --extra-experimental-features nix-command path-info --all --json \
      | jq -r 'to_entries[] | select(.value.references == []) | "\(.key) \(.value.narHash)"' \
      | while read -r p h; do
          p=/nix/store/''${p##*/}
          n=''${p#/nix/store/*-}
          if [ "$(nix-store --print-fixed-path --recursive sha256 "$h" "$n")" = "$p" ]; then
            h32=$(nix --extra-experimental-features nix-command hash convert --hash-algo sha256 --to nix32 "$h")
            echo "update ValidPaths set ca = 'fixed:r:sha256:$h32' where path = '$p';"
          fi
        done | sqlite3 /nix/var/nix/db/db.sqlite
  '';
in
pkgs.testers.runNixOSTest {
  name = "iets-${name}";
  containers.machine = { lib, ... }: {
    nix.package = pkgs.nix;
    nix.settings = {
      experimental-features = [ "nix-command" "flakes" ];
      substituters = lib.mkForce [ ];
      trusted-users = [ "alice" ];
    };
    nix.nixPath = lib.mkForce [ ];
    nix.channel.enable = false;
    # the daemon must not open the database before the store is in place
    systemd.sockets.nix-daemon.wantedBy = lib.mkForce [ ];
    users.users.alice.isNormalUser = true;
    environment.systemPackages = [ pkgs.nushell pkgs.git pkgs.gnutar pkgs.gzip pkgs.diffutils ] ++ packages;
  };
  testScript = { containers, ... }:
    let
      registration = pkgs.closureInfo {
        rootPaths = offline ++ [ containers.machine.system.build.toplevel writableStore ];
      };
    in
    ''
      machine.wait_for_unit("multi-user.target")
      machine.succeed("${writableStore} ${registration}")
      machine.succeed("systemctl start nix-daemon.socket")
      # a git checkout, as in the dev shell: the scripts read this flake as git+file
      machine.succeed("cp -r ${self} /home/alice/iets && chmod -R u+w /home/alice/iets && chown -R alice /home/alice/iets")
      machine.succeed("runuser -l alice -c 'cd iets && git init -q && git add -A && git -c user.name=e2e -c user.email=e2e@example.com commit -q -m e2e'")
      ${prepare}
      machine.succeed("runuser -l alice -c 'cd iets && IETS=${iets}/bin/iets NIXPKGS=${nixpkgs} E2E_OFFLINE=1 ${profile} ${env} nu scripts/${name}.nu' >&2")
      ${collect}
    '';
}
