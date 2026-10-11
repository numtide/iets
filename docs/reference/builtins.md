# Builtins

iets has every builtin that Nix 2.34.8 has. Some work differently, and a few
fail. This page lists those.

Each row has a status:

- `works`: you get what Nix gives you, apart from what the notes say.
- `partial`: it works for the inputs in the notes, and fails for the rest.
- `fails`: it always stops with an error.

These hold for every builtin:

- **No import from derivation.** If a path is the output of a derivation,
  reading it fails, because evaluation never builds anything. You can still
  `import` a `.drv` file.
- **Classic mode falls back.** When iets does not support a call, a classic
  name such as `nix-instantiate` or `nix` runs the real Nix for you
  (`docs/reference/classic.md`).
- **Pure evaluation reads your project.** A pure run of a native command can
  read files below the project root. Nix does not allow that
  (`docs/reference/divergences.md`).

When a note mentions a difference, `docs/reference/divergences.md` has the
details. To add a builtin, see `docs/how-to/add-a-builtin.md`.

## Status

Every builtin that is not in this table works as it does in Nix.

| name | status | notes |
|---|---|---|
| `break` | works | Forces its argument. Nix returns the argument unforced. |
| `currentSystem` | works | Not there in pure evaluation, as in Nix. |
| `currentTime` | works | Not there in pure evaluation, as in Nix. |
| `derivation` | works | See `derivationStrict`. |
| `derivationStrict` | works | An `env` name, `builder` and `system` must be UTF-8. Nix accepts any bytes. |
| `div` | works | A `let` binding of `__div` does not change `/`. In Nix it does. |
| `fetchClosure` | partial | Needs a nix-daemon as the eval store (`store` is `daemon`, `unix://` or `ssh-ng://`). iets reads the binary cache and gives the paths to the daemon. The daemon checks the signatures, as it does for Nix. A NAR compressed with other than `xz`, `zstd`, `bzip2`, `gzip` or `none` fails. Under a classic name it needs the `fetch-closure` feature, as in Nix. |
| `fetchGit` | works | iets copies a local working tree without a `ref` or a `rev` itself, like a flake's own tree. It also reads a commit of a local repository by `rev` or `ref` itself, without `submodules`. Every other fetch runs the real `nix`, which must be on `PATH`. In pure evaluation, an SCP-like URL (`user@host:path`) without a `rev` fails. |
| `fetchMercurial` | fails | |
| `fetchTarball` | partial | Needs a `sha256`. |
| `fetchTree` | partial | Type `path` works for an absolute path. Types `github`, `gitlab`, `sourcehut`, `git`, `tarball` and `hg` need a `narHash`. Other types fail. In pure evaluation, `path` also needs a `narHash`, and every type but `path` and `tarball` needs a `rev`. Under a classic name it needs the `fetch-tree` or `flakes` feature, as in Nix. |
| `fetchurl` | partial | Needs a `sha256` in every mode. Nix needs one only in pure evaluation. `iets prefetch URL` prints it. A hash mismatch exits with 1. Nix exits with 102. |
| `flakeRefToString` | works | Accepts some references that Nix refuses. A `git` input that is not a URL fails. |
| `fromJSON` | works | Error text differs. Refuses nesting deeper than 100,000 levels. |
| `fromTOML` | works | TOML 1.0, as in Nix. Datetimes need the `parse-toml-timestamps` feature. Error text differs. |
| `genericClosure` | works | When two keys cannot be compared, the error can name another pair. |
| `getContext` | works | Fails on the output of a dynamic derivation. Nix builds the producer first. |
| `getFlake` | partial | Impure evaluation only. Needs a local flake with a complete `flake.lock`. |
| `import` | works | Also imports a `.drv` file. |
| `langVersion` | works | `6`. |
| `lessThan` | works | A `let` binding of `__lessThan` does not change `<`. In Nix it does. |
| `mul` | works | A `let` binding of `__mul` does not change `*`. In Nix it does. |
| `nixVersion` | works | `"2.34.8"`. |
| `outputOf` | works | A producer that this run did not make must be in `/nix/store`. Under a classic name it needs the `dynamic-derivations` feature, as in Nix. |
| `parseFlakeRef` | works | Accepts some references that Nix refuses. `sourcehut:`, `hg+...` and other schemes fail. |
| `sub` | works | A `let` binding of `__sub` does not change `-`. In Nix it does. |
| `toJSON` | works | Writes a float with the fewest digits. Nix sometimes writes more. |
