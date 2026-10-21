#!/usr/bin/python3
# external-builders helper: run one build in a per-build guest, unpack the outputs into realStoreDir.
import json, os, shlex, subprocess, sys

TOOLS = os.path.dirname(os.path.abspath(__file__))
KERNEL = "/nix/store/g1bf6r0vzx9rq7w3963agr5qinbbnbjq-linux-6.18.45/Image"
INITRD = "/nix/store/y2pqbmakgv3ifrp5ghiz6y21kj6wxvx0-vm-initrd"
NIX = "/nix/store/c7yzcw6484sgpdfsii4ppjp87djdgh6y-nix-2.34.8"
ROSETTA_BINFMT = (r":rosetta:M::\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00"
                  r":\xff\xff\xff\xff\xff\xfe\xfe\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff"
                  r":/run/rosetta/rosetta:FP")

j = json.load(open(sys.argv[-1]))
top = j["topTmpDir"]
x86 = j["system"] == "x86_64-linux"
run = ["set -e", "busybox --install -s /bin",
       "ln -s /proc/self/fd /dev/fd; ln -s /proc/self/fd/0 /dev/stdin; ln -s /proc/self/fd/1 /dev/stdout; ln -s /proc/self/fd/2 /dev/stderr",
       "mkdir -p /dev/shm /dev/pts /etc; mount -t tmpfs tmpfs /dev/shm; mount -t devpts devpts /dev/pts",
       "echo root:x:0:0::/:/noshell > /etc/passwd; echo nixbld:x:30001:30000::/build:/noshell >> /etc/passwd; echo nixbld:!:30000: > /etc/group",
       *(["mkdir -p /run/rosetta", "mount -t virtiofs rosetta /run/rosetta",
          "mount -t binfmt_misc binfmt_misc /proc/sys/fs/binfmt_misc",
          f"echo '{ROSETTA_BINFMT}' > /proc/sys/fs/binfmt_misc/register"] if x86 else []),
       "chgrp 30000 /nix/store; chmod 1775 /nix/store",
       "cp -a /job/build/. /build/; chown -R 30001:30000 /build; cd /build",
       "set +e",
       "env -i " + " ".join(shlex.quote(f"{k}={v}") for k, v in j["env"].items())
       + " /bin/busybox chpst -u 30001:30000 " + " ".join(map(shlex.quote, [j["builder"]] + j["args"])) + " 2>&1",
       "rc=$?; [ $rc = 0 ] || exit $rc", "set -e"]
for name, p in j["outputs"].items():
    run.append(f"if [ -e {p} ]; then NIX_STATE_DIR=/tmp/ns NIX_CONF_DIR=/tmp {NIX}/bin/nix-store --dump {p} > /job/out-{name}.nar; fi")
open(os.path.join(top, "run"), "w").write("\n".join(run) + "\n")

sys.stderr.write("\2\n")
sys.stderr.flush()
subprocess.run([os.path.join(TOOLS, "vmrun"), KERNEL, INITRD, j["realStoreDir"], top,
                os.environ.get("VM_MEM_MIB", "4096"), os.environ.get("VM_CPUS", "4")] + (["rosetta"] if x86 else []),
               stderr=subprocess.DEVNULL)
if os.path.exists(os.path.join(top, "log")):
    sys.stderr.write(open(os.path.join(top, "log"), errors="replace").read())
status = open(os.path.join(top, "status")).read().strip() if os.path.exists(os.path.join(top, "status")) else "none"
if status != "0":
    sys.exit(f"guest build failed: status {status}")
for name, p in j["outputs"].items():
    nar = os.path.join(top, f"out-{name}.nar")
    if os.path.exists(nar):
        real = os.path.join(j["realStoreDir"], os.path.basename(p))
        subprocess.run(["/run/current-system/sw/bin/nix-store", "--restore", real], stdin=open(nar, "rb"), check=True)
        os.unlink(nar)
