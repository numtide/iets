//! `iets parse`: the parser run over whole trees.

use crate::cli::{help, render};
use crate::syntax::ast::FileId;
use crate::syntax::symbol::Interner;
use crate::syntax::{dump, line_col, parse};
use crate::{error, log};

use std::time::Instant;
use std::{fs, io};

/// What `iets parse` prints for a file that parses.
#[derive(Clone, Copy, PartialEq)]
pub enum Form {
    /// S-expressions.
    Text,
    /// One `{"file", "root"}` line (docs/reference/syntax-tree.md).
    Json,
    /// Nothing; the totals follow the last file.
    Quiet,
}

/// `iets parse [-q | --json] [--log-format F] FILE...`; a lone `-` reads
/// the list of files from stdin.
pub fn main(argv: &[String]) -> i32 {
    let mut form = Form::Text;
    let mut names: Vec<String> = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let s = argv[i].as_str();
        match s {
            "-q" => form = Form::Quiet,
            "--json" => form = Form::Json,
            "--log-format" => {
                i += 1;
                let f = help::value(argv, i, s);
                if let Err(m) = log::set_format(f) {
                    help::arg_error(m)
                }
            }
            _ if s.starts_with('-') && s != "-" => help::unknown_flag(s),
            _ => names.push(s.to_string()),
        }
        i += 1;
    }
    let files: Vec<String> = match names.as_slice() {
        [] => help::arg_error("parse needs FILE... or - to read them from stdin"),
        [dash] if dash == "-" => io::read_to_string(io::stdin())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect(),
        _ => names,
    };
    cmd_parse(&files, form)
}

/// With `-q`, parse every file, report errors, and print totals. Used to
/// check and time the parser on whole trees such as nixpkgs.
pub fn cmd_parse(files: &[String], form: Form) -> i32 {
    let mut interner = Interner::new();
    let mut errors = 0usize;
    let mut bytes = 0usize;
    let start = Instant::now();
    for (i, file) in files.iter().enumerate() {
        let src = match fs::read(file) {
            Ok(s) => s,
            Err(e) => {
                error::Error::io(format!("cannot read {file}"), e).report();
                errors += 1;
                continue;
            }
        };
        bytes += src.len();
        match parse(&src, FileId(i as u32), &mut interner) {
            Ok(ast) => match form {
                Form::Text => {
                    render::bytes(format!("{}\n", dump::dump(&ast, &interner)).as_bytes())
                }
                Form::Json => {
                    let mut line = format!("{{\"file\":{},\"root\":", serde_json::json!(file));
                    dump::json(&dump::tree(&ast, &interner), &mut line);
                    render::bytes(format!("{line}}}\n").as_bytes());
                }
                Form::Quiet => {}
            },
            Err(e) => {
                let (line, col) = line_col(&src, e.span.start);
                log::Report {
                    pos: Some(format!("{file}:{line}:{col}")),
                    ..log::Report::new("parse", e.msg)
                }
                .emit();
                errors += 1;
            }
        }
    }
    if form == Form::Quiet {
        let dt = start.elapsed();
        eprintln!(
            "{} files, {} bytes, {} errors, {:.3}s",
            files.len(),
            bytes,
            errors,
            dt.as_secs_f64()
        );
    }
    if errors == 0 { 0 } else { 1 }
}
