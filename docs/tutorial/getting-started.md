# Get started with iets

iets is a Nix evaluator that gives the same answers as Nix, only faster. In
about ten minutes you'll build it, point it at nixpkgs, build something, and
see why the second run of anything is nearly free.

You need a working `nix` with flakes, and a running `nix-daemon`.
Stay in one terminal and run the commands in order, because later steps use files and variables from earlier ones.

iets is tested against Nix 2.34.8, and "the same answers" means the answers
of that release. Newer releases, such as Nix 2.35, aren't tested. The places
where iets knowingly differs are in
[Known divergences from Nix 2.34](../reference/divergences.md).

## Build iets

From the repository root:

```sh
nix develop -c cargo build --release
```

Go get a coffee: the first build takes a few minutes. When it's done, put
the binary on your path for this shell:

```sh
export PATH="$PWD/target/release:$PATH"
```

Check that it works:

```sh
iets version
```

You get the version:

```
iets 0.1.0
```

Later, when you want iets without a checkout, Nix can build it from the
flake. `nix run` and `nix profile`.

```sh
nix run github:numtide/iets -- version    # run it once
nix profile add github:numtide/iets       # keep it on your PATH
nix build github:numtide/iets#static      # one static binary, with no runtime dependencies (only on Linux)
```

The static binary is described in
[The static build](../explanation/static-build.md).

## Evaluate an expression

```sh
iets eval --strict -E '1 + 1'
```

```
2
```

`-E` takes the expression from the command line. `--strict` evaluates the
whole value instead of stopping at its top level.

## Get nixpkgs

The next steps read nixpkgs. Ask iets for the tree that this repository's
`flake.lock` names, so your output matches what's printed here:

```sh
nixpkgs=$(iets eval --raw --impure -E "(builtins.getFlake \"path:$PWD\").inputs.nixpkgs.outPath")
echo "$nixpkgs"
```

```
/nix/store/hhfnpma32czw4h3bqpag8dciax0bcmar-source
```

If the lock file has moved on, your hash will differ. That's fine, because
the rest of the tutorial uses the variable.

## Evaluate a file

Make a working directory and write a small file that uses nixpkgs:

```sh
mkdir -p ~/iets-tour && cd ~/iets-tour
```

```sh
cat > hello.nix <<'EOF'
let
  pkgs = import <nixpkgs> { };
in
{
  inherit (pkgs.hello) name;
  drv = pkgs.hello.drvPath;
}
EOF
```

iets evaluates purely by default: it reads only your project, and it has
neither a `<nixpkgs>` search path nor `builtins.currentSystem`. nixpkgs
needs both, so every command from here on adds `--impure`.

Now evaluate the file. `-I` tells iets where `<nixpkgs>` is:

```sh
iets eval --impure --strict --json -I nixpkgs=$nixpkgs hello.nix
```

```
{"drv":"/nix/store/2bkip97lh6y3dws9inyhjssyayh9vfhq-hello-2.12.3.drv","name":"hello-2.12.3"}
```

Nix 2.34.8 prints the same derivation path for the same tree. That's the
point: same input, same `.drv`.

Select one attribute with `-A`:

```sh
iets eval --impure --raw -I nixpkgs=$nixpkgs hello.nix -A name
```

```
hello-2.12.3
```

You can also point `-A` at the nixpkgs tree and skip the wrapper file:

```sh
iets eval --impure --raw $nixpkgs -A hello.name
```

```
hello-2.12.3
```

## Build a derivation

Write the smallest derivation you can: it writes one line to one file.

```sh
cat > greeting.nix <<'EOF'
derivation {
  name = "greeting";
  system = builtins.currentSystem;
  builder = "/bin/sh";
  args = [ "-c" "echo hello from iets > $out" ];
}
EOF
```

Build it on your local daemon:

```sh
iets build --impure --builder daemon greeting.nix
```

```
▶ building greeting (/home/you/iets-tour/greeting.nix:2:3)
✔  greeting (/home/you/iets-tour/greeting.nix:2:3) (0s)
/nix/store/hl2d12ai352g88ad47fh8844g939iyrd-greeting
```

Store paths depend on your system. The ones on this page are for
`x86_64-linux`. The daemon's own log lines may show up between the two
progress lines.

iets evaluated the derivation in memory, sent it to the daemon, and asked
the daemon to build it, writing nothing to `/nix/store` itself. Run the
command again and you get only the output path, because the daemon already
has it.

The progress lines name the file and line that defined the derivation. When
a build fails, that's the line you want. Read the output:

```sh
cat /nix/store/hl2d12ai352g88ad47fh8844g939iyrd-greeting
```

```
hello from iets
```

## The cache

`--timings` shows what each phase cost and whether the cache answered. Give
it a cache directory of your own so this step starts cold. Your counts and
times will differ a little from the ones here:

```sh
iets eval --impure --timings --cache-dir ~/iets-tour/cache \
  --strict --json -I nixpkgs=$nixpkgs hello.nix
```

```
cache: miss (no entry)
{"drv":"/nix/store/2bkip97lh6y3dws9inyhjssyayh9vfhq-hello-2.12.3.drv","name":"hello-2.12.3"}
eval: 0.499s
cache: wrote 1218 entries, 0.001s
cache: swept 0 of 1 entries (0 KiB) from /home/you/iets-tour/cache/trace
```

The run wrote down 1,218 things it looked at: every file it read, every
directory it listed, every path it checked.

Run the same command again:

```sh
iets eval --impure --timings --cache-dir ~/iets-tour/cache \
  --strict --json -I nixpkgs=$nixpkgs hello.nix
```

```
cache: hit, 1218 entries validated, 0 rehashed, load 0.000s, validate 0.005s
{"drv":"/nix/store/2bkip97lh6y3dws9inyhjssyayh9vfhq-hello-2.12.3.drv","name":"hello-2.12.3"}
```

There's no `eval:` line, because nothing was evaluated. iets checked those
1,218 entries in a few milliseconds, saw that nothing had changed, and
handed back the saved answer.

Now touch the file and run it a third time. The modification time changes
but the contents don't:

```sh
touch hello.nix
iets eval --impure --timings --cache-dir ~/iets-tour/cache \
  --strict --json -I nixpkgs=$nixpkgs hello.nix
```

```
cache: hit, 1218 entries validated, 1 rehashed, load 0.002s, validate 0.012s
{"drv":"/nix/store/2bkip97lh6y3dws9inyhjssyayh9vfhq-hello-2.12.3.drv","name":"hello-2.12.3"}
```

`1 rehashed` means iets noticed the new modification time and hashed
`hello.nix` again. The contents were the same, so it was still a hit. Change
the contents and the next run misses.

## Classic mode

The same binary answers to Nix's own command names, so your scripts don't
have to change. Instantiate your derivation with one:

```sh
iets nix-instantiate greeting.nix
```

```
warning: you did not specify '--add-root'; the result might be removed by the garbage collector
/nix/store/6jq7mz1b1n11y522nzjdqyvr3gwb69zz-greeting.drv
```

Ask the real Nix the same thing:

```sh
nix-instantiate greeting.nix
```

```
warning: you did not specify '--add-root'; the result might be removed by the garbage collector
/nix/store/6jq7mz1b1n11y522nzjdqyvr3gwb69zz-greeting.drv
```

Same warning, same path, byte for byte. Under those names iets does what it
can and hands everything else to the real `nix`.

## Next steps

- Send builds to another machine:
  [Build on a remote builder](../how-to/build-remotely.md).
- Swap iets in for the Nix tools you use:
  [Replace your Nix tools with iets](../how-to/replace-nix-tools.md).
- Find out why it's fast: [How iets evaluates](../explanation/evaluation.md).
- Look up a flag: [Commands](../reference/commands.md).
