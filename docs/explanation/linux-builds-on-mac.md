# Linux builds on a Mac

You have a Mac and a derivation whose system is `aarch64-linux`. You want
`iets build` to put the output in the Mac's own `/nix/store`, with no remote
builder, no account and no virtual machine that you keep running. This page
says whether that can be done soundly, and how. The measurements are from
2026-10-21 on mac02 (Apple M4, 32 GiB, macOS 26.7.1, Nix 2.35.3).

The verdict: it's feasible. A guest per build boots in 40 ms and costs
nothing between builds. The output reaches the host store as one NAR
through the host daemon, not through a shared writable store. A
writable shared store is unsound on a Mac, and the
[case collision](#the-shared-writable-store) shows why. The prototype in
`scripts/linux-on-mac/` built a derivation in a guest and registered it in
mac02's store, with the same NAR hash as the farm's build of that
derivation. It isn't wired to `iets build` yet. The daemon's own hook,
[`external-builders`](#the-daemons-helper-hook), is the shortest way there.

## The design

The host daemon owns the store from start to end. The guest has no store
database and no daemon. Each part does one job:

- **The kernel and the initrd.** spore's `hcloud-aarch64` kernel config
  plus `CONFIG_BINFMT_MISC`: virtio-fs, overlayfs and the virtio console
  are built in, and it loads no modules. The initrd is 1.6 MiB: a static busybox and a
  20-line `/init`. The kernel `Image` is 16 MiB. Both are store paths.
- **The launcher.** A 40-line Swift program on the Virtualization
  framework boots the kernel directly, with no firmware and no disk. It
  shares the host's `/nix/store` read-only under the tag `store`, and one
  empty job directory read-write under the tag `job`. The guest gets no
  network device.
- **The guest.** `/init` mounts the read-only store as the lower layer of
  an overlay, with a tmpfs as the upper layer. The builder writes `$out`
  into the overlay as uid 30001, so the host store is never written. When
  the builder exits, the guest writes `nix-store --dump $out` into the job
  directory and powers off.
- **The import.** The host side scans the NAR for the hash parts of the
  input closure to find the references. Then it hands the NAR to the host
  daemon with `nix-store --import`, which is `AddToStoreNar` on the wire.

The inputs cost nothing to move: the guest reads them where they are. The
output moves once, as a NAR stream into the daemon. That's the remote
builder's copy-back, without a network, a second store or a second
database.

## The shared writable store

Sharing `/nix/store` read-write into the guest looks like the way to skip
the copy. It fails on four counts, and the first one was measured.

- **Case.** The `/nix` volume on mac02 is case-insensitive APFS
  (`diskutil info /nix`: "APFS", not "Case-sensitive APFS"), and Nix runs
  there with `use-case-hack = true`. In the guest, `echo lower > case-a;
  echo upper > case-A` on the writable share left one file, `case-a`,
  holding `upper`. A Linux output with two names that differ only by case
  (kernel headers have several) is corrupted without an error. The daemon
  avoids this when it unpacks a NAR: the prototype's output holds `A` and
  `a~nix~case~hack~1`.
- **Ownership.** The framework writes a shared file as the user who runs
  the launcher. The store is `root:nixbld`, so the launcher would have to
  run as root to write it. Then the guest kernel can change any valid path
  in the store, and every NAR hash in `db.sqlite` becomes a claim nobody
  checks.
- **The database.** The guest can't register a path. The host daemon has
  to, and it has no operation that registers files somebody else put in
  the store. `nix-store --register-validity` does it, but only as root and
  next to the daemon, not through it.
- **Garbage collection.** A path that is in the store but not valid is
  garbage. mac02 collects every night. A half-written output is deleted
  under the guest unless the daemon holds a lock it doesn't know about.

The honest answer is a copy that's merely fast.

## Store ownership

The host daemon owns the store, and the guest only reads it. This answers
both halves of the first question.

**Who owns the store during the build.** The host daemon. The guest sees
the store through a read-only share and builds into its own memory. A
daemon in the guest is a build front end with the host daemon as its
upstream, and it never opens the host's database
([The guest daemon](#the-guest-daemon)). Only
`AddToStoreNar` makes the output valid, and the daemon checks the NAR hash
and applies the case hack on the way in.

**What happens when the guest dies.** Nothing reaches the store. The
prototype killed the launcher with SIGKILL two seconds into a 30-second
build. Afterwards the output wasn't valid, `/nix/store` had no partial
path, and no launcher process was left. The job directory is the only
leftover, and it's outside the store.

The driver must also hold a temporary root on its daemon connection for
the input closure while the guest runs. Without one, a garbage collection
during the build can delete an input the guest is reading.

## Cost of a guest

A guest per build is cheap enough that keeping one warm buys nothing.

| measurement | value |
|---|---|
| kernel start to `/init` | 0.04 to 0.05 s (guest uptime) |
| boot, trivial build, power off | 0.16 to 0.23 s wall on the host |
| launcher resident memory | 9 MiB |
| guest memory | 1 GiB ceiling, 2 vCPUs, set per build |
| 512 MiB output, build to valid path | 1.15 s in the guest, then 3.46 s import |

The guest memory runs in the framework's own service process, so the
launcher's 9 MiB doesn't include it. That process wasn't measured.

The 3.46 s import, about 150 MiB/s, is the prototype's script. It reads
the NAR from the job directory into memory, builds the export stream and
pipes it to `nix-store`. Writing the NAR into the job directory took about
0.9 s of the guest's 1.05 s. A real driver should stream the NAR from the
guest over a vsock straight into `AddToStoreNar`, so the output is written
once.

The output lives in a tmpfs, so a build's output and its temporary files
must fit in the guest's memory. A sparse disk image as the overlay's upper
layer removes that limit, at the cost of one more file per build.

## x86_64-linux on Apple silicon

Support it through Rosetta, in the same aarch64 guest. The framework
shares the Rosetta runtime into the guest as a directory
(`VZLinuxRosettaDirectoryShare`), and the guest registers it with
`binfmt_misc` for x86_64 ELF files. mac02 reports Rosetta for Linux as
installed (`availability` 2).

The guest registers Rosetta with the flags `FP`. `F` opens the
interpreter at registration, so a build sandbox doesn't need to see it.
`P` keeps `argv[0]`. Without `P`, Rosetta replaces `argv[0]` with the
path it was given. GNU Hello's `operand-1` test then printed `./hello:`
where it expects `hello:`, and its `atexit-1` test failed as well. With
`FP`, both pass.

mac02 rebuilt `hello` 2.12.3 for `x86_64-linux` from source in the guest
(4 vCPUs, 4 GiB). Configure, compile and the test suite ran under
translation in 172 s, and every test passed except the one that skips by
design. The output's NAR hash, `sha256-gqssVX3zprSd1xA4Yc1DOZGdGvtw/YBxhUO06ynY4dg=`,
is the one the public NixOS binary cache holds for the same store path,
which x86_64 hardware built.

An output built under translation gets the same store path as one built
on x86_64 hardware. A translation bug would therefore end up in a path
that looks native, and one matching package doesn't rule that out. So
`x86_64-linux` should be a separate system feature that the user can see,
and the farm should rebuild a sample natively to check it. An Intel Mac
can't run an aarch64 guest, and it gets no `aarch64-linux` support.

## What a user installs

On a Mac with Nix, where the user is in `trusted-users`, the user installs
nothing. Each part arrives as follows:

- **The launcher** is a store path. An ad-hoc signature with the
  `com.apple.security.virtualization` entitlement is enough to start a
  guest. mac02 ran the prototype after `codesign -s -`, with no developer
  account and with System Integrity Protection on.
- **The kernel and the initrd** are store paths too, 18 MiB together.
- **Rosetta**, for `x86_64-linux` only, is part of macOS. If it's
  missing, the launcher calls `installRosetta()`, and the system asks the
  user to agree to its license.

The trust is the part that doesn't land by itself. An input-addressed
output that nobody signed is accepted by the daemon only from a trusted
user. A user who isn't trusted needs one of these:

- a `trusted-users` line in `nix.conf`;
- a signing key that the daemon trusts, which is also configuration;
- content-addressed derivations, which the daemon checks by their
  content and accepts from anyone.

The answer is "nothing" for an admin who installed Nix and is trusted, and
one line of configuration otherwise.

## The daemon's helper hook

Nix 2.32 and later have an experimental hook for this, `external-builders`.
For a derivation whose system the setting lists, the host daemon runs a
helper program instead of the builder. It passes the path of a JSON file
that holds the builder, the arguments, the full environment, the input
closure (`inputPaths`), the output paths, the store directory and a build
directory on the host. The daemon does everything else as for a local
build: scheduling, substitution, locks, temporary roots, structured
attributes, `passAsFile`, `exportReferencesGraph`, reference scanning,
output checks and registration. The helper writes each output at its
path in `realStoreDir` and exits with the build's status.

This makes the guest daemon and a front end unnecessary. The helper is
the launcher plus the guest from [The design](#the-design):

- It shares `realStoreDir` read-only and the build directory into the
  guest, and runs the build there.
- It gets each output back as a NAR and unpacks it with
  `nix-store --restore`. On a Mac this applies `use-case-hack`, and the
  daemon's dump reverses it, so the NAR hash is the guest's.
- It writes `\2` and a newline to standard error when the build starts,
  as the daemon expects from a builder.

The daemon starts the helper as a build user, such as `_nixbld1`. On
mac02, `_nixbld1` started a guest with the Rosetta share, with no user
session and `HOME=/var/empty`.

The cost is configuration. Two lines of `nix.conf`, which only root can
edit, and a daemon restart:

```
extra-experimental-features = external-builders
external-builders = [{"systems": ["aarch64-linux", "x86_64-linux"], "program": "/nix/store/…/bin/helper", "args": []}]
```

Any client of the daemon then builds Linux derivations, `iets build
--builder daemon` included, and nothing needs `trusted-users`. A daemon
that runs a hook it was configured to run trusts nothing new.

`scripts/linux-on-mac/helper.py` is this helper as a prototype. It hasn't
run behind a daemon: the test needs a second daemon with its own store on
mac02, and that wasn't done.

## The guest daemon

This section is the alternative to [the hook](#the-daemons-helper-hook) for a
daemon without it.


The prototype's driver script does a daemon's job badly. The guest should
run a real worker-protocol daemon with a classic backend and a sandbox,
and the launcher should expose it to iets as a Unix socket. Then iets
reaches it with the `unix://` builder it already has, and iets gains no
local builds and no daemon of its own.

The daemon must not share the host's database. Locks don't cross the
share. In a test on mac02, the host held `flock` on one file in the
shared directory and `fcntl` on another, and the guest then took `flock`
on both at once. Both are locks that SQLite depends on. Nix also runs
SQLite in WAL mode, and WAL keeps its index in a shared-memory file. Two
kernels can't share that memory over virtio-fs without DAX, and the
framework offers no DAX window. The case collision in
[The shared writable store](#the-shared-writable-store) rules out the
store files as well. So the guest daemon writes nothing that the host
reads as a store or a database.

The daemon is a build front end over an upstream store. These are the
requirements for it:

- **Upstream store.** The host daemon is the upstream, reached through a
  socket that the launcher forwards over vsock. Path info (validity,
  references, NAR hash) comes from the upstream. File contents come from
  the read-only store share, at the same `/nix/store` paths.
- **Writes.** `AddToStore`, `AddToStoreNar` and `AddMultipleToStore` go to
  the upstream. A built output goes to the upstream as one
  `AddToStoreNar` after the build succeeds, and only then is it valid
  anywhere.
- **Local state.** Outputs being built live in an overlay. The upper
  layer is guest memory or a guest disk image, never the share, so it is
  case-sensitive. A database, if the backend needs one, lives there too.
  It holds only in-flight paths and is thrown away with the guest.
- **Locking mode.** Normal POSIX locks and WAL on the guest-local
  database. The daemon refuses to open a database on a FUSE file system
  (`statfs` magic `0x65735546`, which virtio-fs reports), so a wrong
  configuration fails at start instead of corrupting a store.
- **The big lock.** Only the host daemon holds the garbage-collector lock
  and collects. The guest daemon never collects. For the input closure,
  it asks the upstream for temporary roots (`AddTempRoot`) on its
  connection, which the host daemon drops when the connection closes.
- **Guest death.** Nothing to recover. The upstream connection closes, the
  host daemon drops the temporary roots, and no output was added. The
  next build boots a new guest.
- **Uids.** The guest's uids never reach the host, because the guest writes
  no host file. Builds run as the guest's build users, under the rootless
  sandbox, inside the guest.
- **Rosetta.** For `x86_64-linux`, the guest registers Rosetta with
  `binfmt_misc` flags `FP` before the first build, and the sandbox needs
  no path to it.
- **Lifetime.** One guest per connection. The launcher boots it on
  `accept` and stops it on close. The 0.16 s boot makes keeping a guest
  warm unnecessary.

The guest also needs the parts of a Nix build that the prototype skips.
A real sandbox gives them: `__structuredAttrs`, `exportReferencesGraph`,
`passAsFile`, the reference checks, `/etc/passwd` and `/dev/shm`, and a
network device only for fixed-output derivations.

The prototype shares the whole store, so the builder can read paths
outside its closure. The guest daemon's sandbox closes that: it
bind-mounts only the closure into the build.

With that daemon, the user runs `iets build --builder
unix:///<launcher socket>` on a Linux derivation. iets uploads what the
upstream lacks, the guest builds it, and the output is valid in the host
store when the build returns. No part of this is in iets.

## The prototype

The files are in `scripts/linux-on-mac/`:

- `guest.nix` builds the kernel, the initrd and the test derivation on an
  `aarch64-linux` builder.
- `vmrun.swift` is the launcher. Build it with `swiftc -O vmrun.swift -o
  vmrun`, then sign it with `codesign -s - -f --entitlements
  vm.entitlements vmrun`.
- `vmbuild.py DRV` boots a guest, runs the build, imports the outputs and
  prints the timings. `--kill-after SECONDS` kills the guest, for the
  ownership test. `VM_CPUS` and `VM_MEM_MIB` size the guest. An
  `x86_64-linux` derivation gets the Rosetta share. The driver writes
  `.attrs.json` and `.attrs.sh` for structured attributes.

The run on mac02 that this page describes:

```
$ python3 vmbuild.py /nix/store/hq5dclxn7payik5hsv7rj2ll80q65prk-iets-vm-proof.drv
rw share, case pair: case-a -> case-a holds 'upper'
kernel: 6.18.45 aarch64
wall 0.22s, launcher max rss 9 MiB
boot-uptime 0.04
end-uptime 0.12
import 0.03s
/nix/store/ls1zm3c8yaa4zl975dzkni1jr24av5fi-iets-vm-proof
$ nix path-info --json --json-format 1 /nix/store/ls1zm3c8yaa4zl975dzkni1jr24av5fi-iets-vm-proof
{"…-iets-vm-proof":{"narHash":"sha256-8N9LCiva0nX2W1OrYKi1KjMEIxzJUGz8cOX01+ddmL4=",
 "references":["/nix/store/a9kda025a17126f14c01xpwscmjqddwb-bash-interactive-5.3p15"],…}}
```

The farm built the same `.drv` on an `aarch64-linux` builder, and got the
same `narHash`.
