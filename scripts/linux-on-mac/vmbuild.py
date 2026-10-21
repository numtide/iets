#!/usr/bin/env python3
# vmbuild.py DRV [--kill-after SECONDS]: build a Linux derivation in a per-build guest
# and add the output to the host store through the host daemon.
import json, os, re, shlex, signal, subprocess, sys, tempfile, time

KERNEL = "/nix/store/g1bf6r0vzx9rq7w3963agr5qinbbnbjq-linux-6.18.45/Image"
INITRD = "/nix/store/y2pqbmakgv3ifrp5ghiz6y21kj6wxvx0-vm-initrd"
NIX = "/nix/store/c7yzcw6484sgpdfsii4ppjp87djdgh6y-nix-2.34.8"
CPUS = os.environ.get("VM_CPUS", "2")
MEM_MIB = os.environ.get("VM_MEM_MIB", "1024")
# the ELF header of an x86_64 executable. F opens the interpreter at registration; P keeps
# argv[0], which Rosetta otherwise replaces with the path (hello's operand-1 test fails)
ROSETTA_BINFMT = (r":rosetta:M::\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00"
                  r":\xff\xff\xff\xff\xff\xfe\xfe\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff"
                  r":/run/rosetta/rosetta:FP")
HERE = os.path.dirname(os.path.abspath(__file__))

def sh(*a):
    return subprocess.run(a, check=True, capture_output=True, text=True).stdout

drv_path = sys.argv[1]
kill_after = float(sys.argv[3]) if len(sys.argv) > 3 and sys.argv[2] == "--kill-after" else None
shown = json.loads(sh("nix", "derivation", "show", drv_path))
drv = shown.get("derivations", shown)
drv = next(iter(drv.values()))
outputs = {n: o["path"] if o["path"].startswith("/") else "/nix/store/" + o["path"] for n, o in drv["outputs"].items()}
env = dict(drv["env"])
env.update({k: v for k, v in outputs.items()})
env.update(NIX_BUILD_TOP="/build", TMPDIR="/build", TEMPDIR="/build", TMP="/build", TEMP="/build",
           HOME="/homeless-shelter", PATH="/path-not-set", NIX_STORE="/nix/store",
           NIX_BUILD_CORES=CPUS, NIX_LOG_FD="2", TERM="xterm-256color")
attrs = drv.get("structuredAttrs")
if attrs is not None:
    attrs = dict(attrs, outputs=outputs)
    env.update({k: str(v) for k, v in attrs.get("env", {}).items()})
    env.update(NIX_ATTRS_JSON_FILE="/build/.attrs.json", NIX_ATTRS_SH_FILE="/build/.attrs.sh")

def simple(v):
    if isinstance(v, bool):
        return "1" if v else ""
    if v is None:
        return ""
    if isinstance(v, (str, int, float)):
        return str(v)
    raise TypeError

def attrs_sh(attrs):
    lines = []
    for k, v in attrs.items():
        if not re.fullmatch(r"[A-Za-z0-9_]+", k):
            continue
        try:
            if isinstance(v, list):
                lines.append(f"declare -a {k}=(" + " ".join(shlex.quote(simple(x)) for x in v) + " )")
            elif isinstance(v, dict):
                lines.append(f"declare -A {k}=(" + " ".join(f"[{shlex.quote(a)}]={shlex.quote(simple(b))}" for a, b in v.items()) + " )")
            else:
                lines.append(f"declare {k}={shlex.quote(simple(v))}")
        except TypeError:
            pass
    return "\n".join(lines) + "\n"

builder = drv["builder"] if drv["builder"].startswith("/") else drv["builder"]

job = tempfile.mkdtemp(prefix="job-", dir=os.path.join(HERE, "jobs"))
os.chmod(job, 0o777)
run = ["set -e", "busybox --install -s /bin",
       "ln -s /proc/self/fd /dev/fd; ln -s /proc/self/fd/0 /dev/stdin; ln -s /proc/self/fd/1 /dev/stdout; ln -s /proc/self/fd/2 /dev/stderr",
       "mkdir -p /dev/shm /dev/pts /etc; mount -t tmpfs tmpfs /dev/shm; mount -t devpts devpts /dev/pts",
       "echo root:x:0:0::/:/noshell > /etc/passwd; echo nixbld:x:30001:30000::/build:/noshell >> /etc/passwd; echo nixbld:!:30000: > /etc/group",
       "echo lower > /job/case-a; echo upper > /job/case-A",
       "echo \"rw share, case pair: $(ls /job | grep -i '^case-a$' | tr '\\n' ' ')-> case-a holds '$(cat /job/case-a)'\"",
       *(["mkdir -p /run/rosetta", "mount -t virtiofs rosetta /run/rosetta",
          "mount -t binfmt_misc binfmt_misc /proc/sys/fs/binfmt_misc",
          f"echo '{ROSETTA_BINFMT}' > /proc/sys/fs/binfmt_misc/register"]
         if drv["system"] == "x86_64-linux" else []),
       "chgrp 30000 /nix/store; chmod 1775 /nix/store; chown 30001:30000 /build; cd /build",
       "set +e",
       "env -i " + " ".join(shlex.quote(f"{k}={v}") for k, v in env.items())
       + " /bin/busybox chpst -u 30001:30000 " + " ".join(shlex.quote(x) for x in [builder] + drv["args"]),
       "rc=$?; [ $rc = 0 ] || exit $rc", "set -e"]
for name, p in outputs.items():
    run.append(f"NIX_STATE_DIR=/tmp/ns NIX_CONF_DIR=/tmp {NIX}/bin/nix-store --dump {p} > /job/{name}.nar")
if attrs is not None:
    json.dump(attrs, open(os.path.join(job, ".attrs.json"), "w"))
    open(os.path.join(job, ".attrs.sh"), "w").write(attrs_sh(attrs))
    run.insert(run.index("set +e"), "cp /job/.attrs.json /job/.attrs.sh /build/ && chown 30001:30000 /build/.attrs.*")
open(os.path.join(job, "run"), "w").write("\n".join(run) + "\n")

t0 = time.monotonic()
vm = subprocess.Popen(["/usr/bin/time", "-l", os.path.join(HERE, "vmrun"), KERNEL, INITRD, "/nix/store", job, MEM_MIB, CPUS] + (["rosetta"] if drv["system"] == "x86_64-linux" else []),
                      stderr=open(os.path.join(job, "vm.stderr"), "w"))
if kill_after:
    time.sleep(kill_after)
    subprocess.run(["pkill", "-9", "-f", "vmrun .* " + job])
vm.wait()
wall = time.monotonic() - t0
print(open(os.path.join(job, "log")).read() if os.path.exists(os.path.join(job, "log")) else "(no guest log)", end="")
rss = re.search(r"(\d+)\s+maximum resident set size", open(os.path.join(job, "vm.stderr")).read())
print(f"wall {wall:.2f}s, launcher max rss {int(rss.group(1)) >> 20 if rss else '?'} MiB")
for f in ("boot-uptime", "end-uptime"):
    if os.path.exists(os.path.join(job, f)):
        print(f, open(os.path.join(job, f)).read().split()[0])
status = open(os.path.join(job, "status")).read().strip() if os.path.exists(os.path.join(job, "status")) else "none"
if status != "0":
    sys.exit(f"guest status {status}; job {job} discarded, nothing registered")

closure = {c for c in sh("nix-store", "-qR", "--include-outputs", drv_path).split() if not c.endswith(".drv")}

def num(n):
    return n.to_bytes(8, "little")

def text(b):
    b = b.encode()
    return num(len(b)) + b + bytes(-len(b) % 8)

t1 = time.monotonic()
export = b""
for name, p in outputs.items():
    nar = open(os.path.join(job, name + ".nar"), "rb").read()
    refs = sorted(c for c in closure | set(outputs.values()) if os.path.basename(c)[:32].encode() in nar)
    export += num(1) + nar + num(0x4558494E) + text(p) + num(len(refs)) + b"".join(map(text, refs)) + text(drv_path) + num(0)
subprocess.run(["nix-store", "--import"], input=export + num(0), check=True, capture_output=True)
print(f"import {time.monotonic() - t1:.2f}s")
for p in outputs.values():
    print(p)
