let
  spore = builtins.getFlake "github:numtide/spore/cdd7f2b17df9c3185e729ea9798384d4d54c05c8";
  pkgs = import spore.inputs.nixpkgs { system = "aarch64-linux"; };
  init = pkgs.writeText "init" ''
    #!/bin/sh
    mount -t proc proc /proc
    mount -t sysfs sys /sys
    mount -t devtmpfs dev /dev
    mount -t virtiofs job /job
    cat /proc/uptime > /job/boot-uptime
    mkdir -p /nix/.ro /nix/.rw /nix/store
    mount -t virtiofs store /nix/.ro -o ro
    mount -t tmpfs -o size=80% tmpfs /nix/.rw
    mkdir -p /nix/.rw/upper /nix/.rw/work /nix/.rw/build
    mount -t overlay overlay -o lowerdir=/nix/.ro,upperdir=/nix/.rw/upper,workdir=/nix/.rw/work /nix/store
    mount -t tmpfs tmpfs /tmp
    sh /job/run > /job/log 2>&1
    echo $? > /job/status
    cat /proc/uptime > /job/end-uptime
    sync
    poweroff -f
  '';
in
{
  kernel = spore.packages.aarch64-linux.hcloud-kernel;
  initrd = pkgs.runCommand "vm-initrd" { nativeBuildInputs = [ pkgs.cpio ]; } ''
    mkdir -p root/bin root/proc root/sys root/dev root/nix root/job root/tmp root/build
    cp ${pkgs.pkgsStatic.busybox}/bin/busybox root/bin/
    for a in sh mount mkdir poweroff cat env chpst sync chown chmod ls echo; do ln -s busybox root/bin/$a; done
    cp ${init} root/init
    chmod +x root/init
    (cd root && find . | cpio -o -H newc --owner=0:0) > $out
  '';
  inherit (pkgs) nix;
  test = derivation {
    name = "iets-vm-proof";
    system = "aarch64-linux";
    builder = "${pkgs.bash}/bin/bash";
    args = [ "-c" ''
      export PATH=${pkgs.coreutils}/bin
      echo "kernel: $(cat /proc/sys/kernel/osrelease) $(uname -m)"
      mkdir -p $out/case
      echo lower > $out/case/a
      echo upper > $out/case/A
      echo hello from an aarch64-linux guest > $out/greeting
      ln -s ${pkgs.bash}/bin/bash $out/sh
    '' ];
  };
}
