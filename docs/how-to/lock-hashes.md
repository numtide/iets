# How to fetch without writing hashes

You keep the hashes of your fetches in `iets.lock`, and your expressions
name only URLs. The file works on Nix too. Why it works this way, and what
you trust when you use it, is in
[explanation/locked-hashes.md](../explanation/locked-hashes.md).

## Set it up once

Copy `nix/lock.nix` from the iets repository into yours. It is plain Nix
and has no inputs. Then load it next to your lock file:

```nix
let lock = import ./nix/lock.nix ./iets.lock; in
```

The lock file does not need to exist yet.

## Fetch a tarball

```nix
lock.fetchTarball "https://github.com/numtide/treefmt/archive/v2.0.0.tar.gz"
```

The first evaluation fails and tells you what to run:

```
error: iets.lock has no unpack hash for https://github.com/numtide/treefmt/archive/v2.0.0.tar.gz.
Check that you trust the URL, then run this in the directory of iets.lock:
  iets lock --unpack https://github.com/numtide/treefmt/archive/v2.0.0.tar.gz
```

Run the command, then evaluate again. Commit `iets.lock`.

## Use a nixpkgs fetcher

`lock.hash` gives you the hash for any fetcher that takes `hash`:

```nix
pkgs.fetchurl {
  inherit url;
  hash = lock.hash { inherit url; };
}

pkgs.fetchzip {
  inherit url;
  hash = lock.hash { inherit url; unpack = true; };
}
```

Use `unpack = true` for the fetchers that unpack an archive. Do not use the
lock for a call with `postFetch`, `stripRoot = false` or other arguments
that change the output. Write the hash in the expression for those.

## When the remote changed

If a URL now serves something else, the fetch fails with a hash mismatch.
Check what changed. When you trust the new content, record it:

```sh
iets lock --unpack --update https://example.org/src.tar.gz
```

## Use another file name

Pass the file to the helper, and give `--file` to `iets lock`:

```nix
let lock = import ./nix/lock.nix ./sources.lock; in
```

```sh
iets lock --file sources.lock https://example.org/data.json
```
