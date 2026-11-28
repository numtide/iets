# Fuzz iets against Nix

`fuzz/` feeds the same input to iets and to Nix 2.34.8 and tells you when
their answers differ. Each target, its kinds of difference and the file
formats are in [Fuzz targets](../reference/fuzz.md).

## Checks

The flake has one check for each family of targets, and one for the
nix-conformance cases:

```sh
nix build .#checks.x86_64-linux.fuzz-parse -L
nix build .#checks.x86_64-linux.fuzz-nar -L
nix build .#checks.x86_64-linux.fuzz-wire -L
nix build .#checks.x86_64-linux.fuzz-eval -L
nix build .#checks.x86_64-linux.fuzz-drv -L
nix build .#checks.x86_64-linux.fuzz-drv-eval -L
nix build .#checks.x86_64-linux.conformance -L
```

A check fails when it finds a difference that `fuzz/divergences.toml` does
not list, and it prints the input in base64.

Each check runs a fixed number of inputs (`runs` in `nix/fuzz.nix`) with a
seed taken from its derivation. A slow builder runs as many inputs as a fast
one, and two runs of one check keep the same inputs. Each target prints the
number and the digest of the inputs it kept, so you can compare two runs.
This holds only while libFuzzer sees the same coverage and the same compared
values for the same input:

- The check runs the fuzzer under `setarch -R`, without address
  randomization, because the evaluator keeps sets keyed on pointers.
- The check puts the scratch trees on tmpfs (`TMPDIR=/dev/shm`). Other file
  systems list a directory in an order of their own.
- Code that a target reaches must not compare a clock reading. Use
  `sys::elapsed` for timings, which is zero in a fuzzing build.
- Code that a target reaches must not use a hash table with a random seed
  (`std::collections::HashMap::new`). Use `FxHashMap`, `BTreeMap` or a
  `Vec`.
- Code that a target reaches must not run in a thread beside the input. The
  wire target runs its scripted daemon as a child process.
- The `-timeout` alarm must not interrupt a system call of the input:
  `restart_on_alarm` (fuzz/src/lib.rs) has the kernel restart it.

The inputs in `fuzz/regressions/drv-read/` once diverged; `fuzz-drv` runs
them each time as seeds.

## Properties of nix-pbt

[nix-pbt](https://github.com/yorickvP/nix-pbt) draws expressions with Hegel
and compares what each evaluator makes of them. `nix-pbt` runs its suites
with the reference Nix first and iets second, on x86_64-linux:

```sh
nix run .#nix-pbt                  # every suite
nix run .#nix-pbt -- arith lists   # some of them
HEGEL_TEST_CASES=2000 nix run .#nix-pbt -- strings -- substring
```

No check runs it. The `arith`, `attrs`, `builtins`, `exprs`, `lists`,
`strings` and `versions` suites pass; the suites of the store, the fetchers and flakes
report differences that nobody has sorted into bugs and known divergences.

## Manual fuzzing

To run a fuzzer yourself you need only the flake's stable Rust. `cargo fuzz`
runs on it with `--sanitizer none`, because AddressSanitizer needs nightly.

```sh
nix build .#fuzz-oracles -o oracles
nix build .#fuzzers -o fuzzers
export IETS_PARSE_ORACLE=$PWD/oracles/bin/parse-oracle
export IETS_NAR_ORACLE=$PWD/oracles/bin/nar-oracle
mkdir -p corpus && cp tests/lang/*.nix corpus/
fuzzers/bin/fuzz-parse -dict=fuzz/parse.dict -max_len=4096 corpus
fuzzers/bin/fuzz-nar-gen -max_len=4096 nar-corpus
```

`nix/fuzz.nix` has the flags of each target. To use N cores and keep
going after a difference, add `-fork=N -ignore_crashes=1`. Each difference is
saved as a file named `crash-<sha1>`.

While you work on iets, build a single target with
`cargo fuzz build -s none -O TARGET` in `fuzz/`.

## Replays

`fuzz-diff` runs files through a target without libFuzzer. Give it a crash
file, a corpus, or a tree of `.nix` files (`--nix-only` reads only the
`*.nix` files of a tree):

```sh
nix build .#fuzz-diff -o fuzz-diff
fuzz-diff/bin/fuzz-diff parse crash-0123abcd
fuzz-diff/bin/fuzz-diff --nix-only parse ~/src/nixpkgs
fuzz-diff/bin/fuzz-diff --json nar-gen corpus/ > triage.jsonl
```

## Differences

A difference looks like this:

```
parse divergence (position)
  nix:  error at 1:1: syntax error, unexpected end of path, expecting '${'
  iets: error at 1:4: syntax error, unexpected end of path, expecting '${'
```

1. Make the input small:
   `fuzzers/bin/fuzz-parse -minimize_crash=1 -runs=10000 crash-0123abcd`.
2. Check it with the real Nix.
   - For the parser, write the input to a file and run
     `nix-instantiate --parse FILE`. Don't use `-E` or standard input: with
     those, Nix finds lines only in the text before the first NUL byte
     (`Pos::getSource`, `position.cc`).
   - For a NAR, run `nix-store --restore DIR < FILE` and
     `nix-store --dump DIR`.
3. Find the rule in the Nix source (`src/libexpr/parser.y`, `lexer.l`,
   `parser-state.hh`, `src/libutil/archive.cc`). Then decide:
   - **iets is wrong.** Fix it, and add a test next to the code. Also add
     the small input to `parse/` or `nar/` of
     [nix-conformance](https://github.com/numtide/nix-conformance), named
     by its sha1 (`NAME.nix` or `NAME.nar`). Then write its expected answer
     from the reference adapter:
     `./expect "$(nix build .#adapter --print-out-paths)/bin/nix-conformance-adapter"`.
     If Nix itself fails on the input, it isn't a case: put it in
     `nix-bugs/` with a note.
   - **iets differs on purpose.** Add a section to
     `docs/reference/divergences.md` and an entry to
     `fuzz/divergences.toml` (see **Record a known divergence**).
   - **The oracle or the target is wrong.** Fix it there.

To see the input of the last run, set `IETS_WIRE_SCRIPT`, `IETS_EVAL_SOURCE`
or `IETS_DRV_WRITE` to a file name and the target writes the input there. To
serve a `wire` script to a client by hand, use for example
`socat UNIX-LISTEN:SOCK,fork SYSTEM:'cat FILE'`.

## Known divergences

Add an entry to `fuzz/divergences.toml`, under the table of the target's
family, with an `id`, a `doc` link to the section of
`docs/reference/divergences.md`, and the fields that make it match
([Fuzz targets](../reference/fuzz.md#fuzzdivergencestoml)).

The fuzzer counts that difference as expected and stops adding it to the
corpus. Keep entries narrow, because a broad one hides bugs. Each binary
uses the list it was built with. To try another list, set
`IETS_FUZZ_DIVERGENCES=FILE`.

## Campaigns

A campaign is a long run of one family on every core. The flake has one
for each family: `fuzz-parse-campaign`, `fuzz-nar-campaign`,
`fuzz-wire-campaign`, `fuzz-eval-campaign` and `fuzz-drv-campaign`. Build
one on a machine with many cores:

```sh
nix build '.#fuzz-parse-campaign' -o campaign
```

A campaign fuzzes for an hour. `campaign/artifacts/` holds the inputs that
differed, `campaign/triage.jsonl` holds those inputs replayed and classified,
and `campaign/corpus/` holds the corpus it grew. To choose the target, the
duration or a starting corpus, call
`(import ./fuzz { ... }).campaign { target; seconds; corpus; }`.

The grown corpora aren't in the repository, so keep the ones you want. The
parse seeds need no storage, because each run takes them from the pinned
nixpkgs.
