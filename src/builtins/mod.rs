//! Primop registration. Each family lives in its own file and exposes
//! `pub fn register(r: &mut Registry)`. See `docs/reference/builtins.md`.

pub mod attrs;
pub mod context;
pub mod core;
pub mod derivation;
pub mod lists;
pub mod math;
pub mod paths;
pub mod store_hooks;
pub mod strings;
pub mod text;

use crate::eval::{Evaluator, Pos};
use crate::runtime::{PrimFn, Word};
use crate::syntax::symbol::Sym;

pub struct Registry {
    pub ops: Vec<(&'static str, u8, PrimFn)>,
    pub consts: Vec<(&'static str, Word)>,
}

impl Registry {
    /// Register `name` (with or without the `__` prefix). Names without the
    /// prefix are also visible as globals; prefixed ones are globals under
    /// their prefixed name and appear in `builtins` unprefixed.
    pub fn op(&mut self, name: &'static str, arity: u8, f: PrimFn) {
        self.ops.push((name, arity, f));
    }

    pub fn constant(&mut self, name: &'static str, w: Word) {
        self.consts.push((name, w));
    }
}

/// Embedded corepkgs, addressed as `<nix/NAME>`.
pub fn corepkgs(name: &[u8]) -> Option<&'static [u8]> {
    match name {
        b"fetchurl.nix" => Some(include_bytes!("../corepkgs/fetchurl.nix")),
        b"derivation.nix" => Some(include_bytes!("../corepkgs/derivation.nix")),
        _ => None,
    }
}

fn builtins_name(name: &'static str) -> &'static str {
    name.strip_prefix("__").unwrap_or(name)
}

/// Whether this run puts `name` in `builtins` at all. Nix leaves three out
/// until an experimental feature is on, and code branches on whether they are
/// there: `flake-compat` takes its `fetchTree` path when `builtins ? fetchTree`.
/// Under one of Nix's own names iets answers as Nix does; its own commands
/// have every feature, which is the documented divergence.
fn exposed(name: &'static str) -> bool {
    if !crate::log::classic() {
        return true;
    }
    match builtins_name(name) {
        // primops.cc: `fetch-tree` or `flakes`, since the flake machinery is
        // written in Nix and calls it
        "fetchTree" => crate::config::xp_on("fetch-tree") || crate::config::xp_on("flakes"),
        "fetchClosure" => crate::config::xp_on("fetch-closure"),
        "outputOf" => crate::config::xp_on("dynamic-derivations"),
        _ => true,
    }
}

pub fn install(ev: &mut Evaluator) {
    let mut r = Registry {
        ops: Vec::new(),
        consts: Vec::new(),
    };
    core::register(&mut r);
    attrs::register(&mut r);
    lists::register(&mut r);
    strings::register(&mut r);
    math::register(&mut r);
    paths::register(&mut r);
    context::register(&mut r);
    text::register(&mut r);
    derivation::register(&mut r);

    r.constant("true", Word::TRUE);
    r.constant("false", Word::FALSE);
    r.constant("null", Word::NULL);
    // eval.cc:addConstant: the `impureOnly` ones do not exist when pure
    if !ev.pure() {
        let sys = ev.mk_string(ev.current_system().as_bytes());
        r.constant("__currentSystem", sys);
    }
    let ver = ev.mk_string(b"2.34.8");
    r.constant("__nixVersion", ver);
    r.constant("__langVersion", Word::imm_int(6));
    let sd = ev.store_dir.clone();
    let sd = ev.mk_string(&sd);
    r.constant("__storeDir", sd);
    let mut items = Vec::new();
    for (prefix, path) in ev.search_path().to_vec() {
        let p = ev.mk_string(&path);
        let pre = ev.mk_string(&prefix);
        let mut e = vec![(ev.s.path, Pos::NONE, p), (ev.s.prefix, Pos::NONE, pre)];
        items.push(ev.arena.mk_attrs(&mut e).unwrap());
    }
    let np = ev.mk_list(&items);
    r.constant("__nixPath", np);
    if !ev.pure() {
        let s = ev.sym(b"currentTimeRaw");
        let ct_op = ev
            .arena
            .mk_primop("currentTimeRaw", 1, text::current_time, s);
        let ct = ev.arena.mk_thunk_app(ct_op, Word::NULL);
        r.constant("__currentTime", ct);
    }

    let mut entries: Vec<(Sym, Pos, Word)> = Vec::new();
    for (name, arity, f) in &r.ops {
        if !exposed(name) {
            continue;
        }
        let gsym = ev.sym(name.as_bytes());
        let w = ev.arena.mk_primop(name, *arity, *f, gsym);
        let bsym = ev.sym(builtins_name(name).as_bytes());
        entries.push((bsym, Pos::NONE, w));
        ev.globals.table.insert(gsym, w);
    }
    for (name, w) in &r.consts {
        let bsym = ev.sym(builtins_name(name).as_bytes());
        entries.push((bsym, Pos::NONE, *w));
        let gsym = ev.sym(name.as_bytes());
        ev.globals.table.insert(gsym, *w);
    }
    let derivation_sym = ev.sym(b"derivation");
    let builtins_sym = ev.sym(b"builtins");
    entries.push((derivation_sym, Pos::NONE, Word::NULL));
    entries.push((builtins_sym, Pos::NONE, Word::NULL));
    let bw = ev
        .arena
        .mk_attrs(&mut entries)
        .expect("builtin names are unique");
    let battrs = bw.as_attrs().unwrap();
    battrs.get(builtins_sym).unwrap().cell.set(bw);
    ev.builtins_word = bw;
    ev.globals.table.insert(builtins_sym, bw);
    ev.globals.builtins = Some(battrs);

    let src = include_str!("../corepkgs/derivation.nix");
    let ir = ev
        .compile_internal(b"\xc2\xabderivation-internal\xc2\xbb", src.as_bytes())
        .expect("derivation.nix compiles");
    let root = ev.root_env;
    let d = ev.eval(root, ir).expect("derivation.nix evaluates");
    battrs.get(derivation_sym).unwrap().cell.set(d);
    ev.globals.table.insert(derivation_sym, d);
}
