//! `iets lock`: the command line of `api::lock`.

use crate::api::lock::lock;
use crate::cli::help::{arg_error, each_arg, unknown_flag};
use crate::cli::plumbing::is_url;

pub const HELP: &str = "\
usage: iets lock [--file FILE] [--unpack] [--update] URL...

Fetch each URL and record its hash in FILE, for nix/lock.nix to read
('iets help commands'). The first fetch is trusted as it is: check what
the URL serves before you commit the file.

  --file FILE   the lock file (default: iets.lock)
  --unpack      hash the NAR of the unpacked tarball (fetchTarball, fetchzip)
  --update      replace an entry whose URL now serves something else
";

pub fn cmd_lock(argv: &[String]) -> i32 {
    let (mut file, mut unpack, mut update) = ("iets.lock".to_string(), false, false);
    let mut urls = Vec::new();
    each_arg(argv, |s, next| match s {
        "--file" => file = next(),
        "--unpack" => unpack = true,
        "--update" => update = true,
        s if is_url(s) => urls.push(s.to_string()),
        s if s.starts_with('-') => unknown_flag(s),
        s => arg_error(format!("not a URL: '{s}'")),
    });
    if urls.is_empty() {
        arg_error("nothing to lock: give a URL")
    }
    match lock(&file, unpack, update, &urls) {
        Ok(()) => 0,
        Err(e) => e.report(),
    }
}
