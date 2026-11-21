# One source per consumer, so an edit rebuilds only what reads it. The binary
# embeds docs/reference (src/cli/docs.rs) and no other page.
{ lib }:
let
  fs = lib.fileset;
  root = ../.;
  sourceOf = files: fs.toSource { inherit root; fileset = fs.unions files; };
  bin = [ ../Cargo.toml ../Cargo.lock ../build.rs ../proto ../src ../docs/reference ];
in
{
  bin = sourceOf bin;
  test = sourceOf (bin ++ [ ../tests ../examples ../.config/nextest.toml ]);
  ab = sourceOf (bin ++ [ ../examples/ab.rs ]);
  bench = sourceOf [ ../bench/Cargo.toml ../bench/Cargo.lock ../bench/eval.rs ];
  fuzz = sourceOf (bin ++ [ ../fuzz ]);
  astGrep = sourceOf [ ../sgconfig.yml ./ast-grep (fs.fileFilter (f: f.hasExt "rs") root) ];
}
