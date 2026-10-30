//! Regular expressions, string rewriting, hashing, JSON/XML/TOML conversion,
//! rounding, `genericClosure`, and environment access.

mod fromtoml;
pub mod json;
pub mod regex;
mod xml;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{self, SystemTime};
use std::{env, str};

use super::Registry;
use crate::builtins::paths;
use crate::builtins::store_hooks::{self, Follow};
use crate::eval::{Error, ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::lint::{coerce, purity};
use crate::log;
use crate::runtime::{Ctx, ListObj, StringObj, Word, ctx_to_arena};
use crate::store::hash::{Hash, HashAlgo, HashFormat, hash_bytes};
use crate::syntax::symbol::FxHashMap;

pub use fromtoml::set_parse_toml_timestamps;

pub fn register(r: &mut Registry) {
    r.op("__match", 2, match_);
    r.op("__split", 2, split);
    r.op("__replaceStrings", 3, replace_strings);
    r.op("__hashString", 2, hash_string);
    r.op("__hashFile", 2, hash_file);
    r.op("__convertHash", 1, convert_hash);
    r.op("__floor", 1, floor);
    r.op("__ceil", 1, ceil);
    r.op("__genericClosure", 1, generic_closure);
    r.op("__getEnv", 1, get_env);
    r.op("break", 1, break_);
    r.op("__toJSON", 1, json::to_json);
    r.op("__fromJSON", 1, json::from_json);
    r.op("__toXML", 1, xml::to_xml);
    r.op("fromTOML", 1, fromtoml::from_toml);
}

/// `builtins.currentTime` is a thunk over this, so the run is marked
/// uncacheable only when a program forces it.
pub(super) fn current_time(ev: &mut Evaluator, _args: &[Word], _pos: Pos) -> EvalResult<Word> {
    ev.trace().mark_uncacheable();
    let now = SystemTime::now()
        .duration_since(time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(Word::imm_int(now))
}

/// One ERE, or why Nix would reject it; the cache keeps the rejection so a
/// repeated bad pattern is not re-parsed. The cache is per thread and holds
/// no run state: a pattern compiles the same way in every session.
type CompiledRegex = Result<regex::Prog, regex::Error>;

thread_local! {
    static REGEX_CACHE: RefCell<FxHashMap<Vec<u8>, Rc<CompiledRegex>>> = RefCell::new(FxHashMap::default());
}

/// Entries in this thread's regex cache (`IETS_STATS`).
#[inline(never)]
pub fn regex_cache_len() -> usize {
    REGEX_CACHE.with(|c| c.borrow().len())
}

fn invalid_regex(re: &[u8], e: regex::Error, pos: Pos) -> Box<Error> {
    let what = match e {
        regex::Error::Invalid => "invalid regular expression",
        regex::Error::TooBig => "memory limit exceeded by regular expression",
    };
    Error::boxed(
        ErrorKind::Eval,
        format!("{what} '{}'", String::from_utf8_lossy(re)),
        pos,
    )
}

// parallel: never inlined so the thread-local is resolved on the current
// thread; callers hold the Rc across no force.
#[inline(never)]
fn get_regex(re: &[u8]) -> Rc<CompiledRegex> {
    REGEX_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(r) = c.get(re) {
            return r.clone();
        }
        let r = Rc::new(regex::compile(re));
        c.insert(re.to_vec(), r.clone());
        r
    })
}

/// Refuses an invalid regex before the string is forced, as prim_match and
/// prim_split do: a bad pattern is an error even where forcing the string
/// would throw one `tryEval` catches. The `Rc` is not held across that force.
fn check_regex(re: &[u8], pos: Pos) -> EvalResult<()> {
    match get_regex(re).as_ref() {
        Ok(_) => Ok(()),
        Err(e) => Err(invalid_regex(re, *e, pos)),
    }
}

fn group_list(
    ev: &mut Evaluator,
    groups: &[Option<(usize, usize)>],
    hay: &'static [u8],
) -> &'static ListObj {
    let out = ev.arena.alloc_list(groups.len());
    for (c, g) in out.cells().iter().zip(groups) {
        c.set(match *g {
            Some((a, b)) => ev.arena.mk_string_static(&hay[a..b], &[]),
            None => Word::NULL,
        });
    }
    out
}

fn match_(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let re = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.match",
    )?;
    check_regex(re, pos)?;
    let s = ev.force_string(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.match",
    )?;
    let cre = get_regex(re);
    let prog = match cre.as_ref() {
        Ok(p) => p,
        Err(e) => return Err(invalid_regex(re, *e, pos)),
    };
    let Some(groups) = regex::match_exact(prog, s.bytes) else {
        return Ok(Word::NULL);
    };
    Ok(Word::from_ref(group_list(ev, &groups, s.bytes)))
}

/// `std::cregex_iterator` order: after an empty match the search resumes one
/// byte later; after a non-empty match it resumes at the match end, so an
/// empty match directly after a non-empty one is reported.
fn split(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let re = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.split",
    )?;
    check_regex(re, pos)?;
    let s = ev.force_string(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.split",
    )?;
    let cre = get_regex(re);
    let prog = match cre.as_ref() {
        Ok(p) => p,
        Err(e) => return Err(invalid_regex(re, *e, pos)),
    };
    let hay = s.bytes;
    let mut items: Vec<Word> = Vec::new();
    let mut prev_end = 0;
    for m in regex::matches(prog, hay) {
        items.push(ev.arena.mk_string_static(&hay[prev_end..m.start], &[]));
        items.push(Word::from_ref(group_list(ev, &m.groups, hay)));
        prev_end = m.end;
    }
    if items.is_empty() {
        return Ok(ev.mk_list(&[Word::from_ref(s)]));
    }
    items.push(ev.arena.mk_string_static(&hay[prev_end..], &[]));
    Ok(ev.mk_list(&items))
}

fn replace_strings(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let from_l = ev.force_list(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.replaceStrings",
    )?;
    let to_l = ev.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.replaceStrings",
    )?;
    if from_l.len != to_l.len {
        bail!(
            ErrorKind::Eval,
            pos,
            "'from' and 'to' arguments passed to builtins.replaceStrings have different lengths"
        );
    }
    let mut from: Vec<&'static [u8]> = Vec::with_capacity(from_l.len as usize);
    for c in from_l.cells() {
        let w = c.get();
        from.push(
            ev.force_string(
                w,
                pos,
                "while evaluating one of the strings to replace passed to builtins.replaceStrings",
            )?
            .bytes,
        );
    }
    let s = ev.force_string(
        args[2],
        pos,
        "while evaluating the third argument passed to builtins.replaceStrings",
    )?;
    if from.is_empty() {
        return Ok(Word::from_ref(s));
    }
    let bytes = s.bytes;
    let mut ctx: Vec<Ctx> = s.ctx.to_vec();
    let mut to: Vec<Option<&'static StringObj>> = vec![None; from.len()];
    let mut res: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut p = 0;
    // one past the end so an empty `from` also matches after the last byte
    while p <= bytes.len() {
        let mut found = false;
        for (j, f) in from.iter().enumerate() {
            if bytes[p..].starts_with(f) {
                found = true;
                let t = match to[j] {
                    Some(t) => t,
                    None => {
                        let w = to_l.cells()[j].get();
                        let t = ev.force_string(
                            w,
                            pos,
                            "while evaluating one of the replacement strings passed to builtins.replaceStrings",
                        )?;
                        ctx.extend_from_slice(t.ctx);
                        to[j] = Some(t);
                        t
                    }
                };
                res.extend_from_slice(t.bytes);
                if f.is_empty() {
                    if p < bytes.len() {
                        res.push(bytes[p]);
                    }
                    p += 1;
                } else {
                    p += f.len();
                }
                break;
            }
        }
        if !found {
            if p < bytes.len() {
                res.push(bytes[p]);
            }
            p += 1;
        }
    }
    let ctx = ctx_to_arena(&mut ev.arena, ctx);
    let out = ev.arena.alloc_bytes(&res);
    Ok(ev.arena.mk_string_static(out, ctx))
}

fn parse_algo(ev: &mut Evaluator, w: Word, pos: Pos, ctx: &str) -> EvalResult<HashAlgo> {
    let name = ev.force_string_no_ctx(w, pos, ctx)?;
    match str::from_utf8(name).ok().and_then(HashAlgo::parse) {
        Some(a) => Ok(a),
        None => bail!(
            ErrorKind::Eval,
            pos,
            "unknown hash algorithm '{}'",
            String::from_utf8_lossy(name)
        ),
    }
}

fn hash_string(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let algo = parse_algo(
        ev,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.hashString",
    )?;
    let s = ev.force_string(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.hashString",
    )?;
    let h = hash_bytes(algo, s.bytes).to_base16();
    Ok(ev.mk_string(h.as_bytes()))
}

fn hash_file(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let algo = parse_algo(
        ev,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.hashFile",
    )?;
    let path = paths::realise_path(ev, args[1], pos)?;
    let follow = Follow::Full;
    let (path, at) = store_hooks::located(ev, path, follow, pos)?;
    let at = store_hooks::check(ev, at, &path, follow, pos)?;
    coerce::read_through_copy(ev, &path, pos)?;
    let data = match at.read(ev, true) {
        Ok(d) => d,
        Err((_, e)) => bail!(
            ErrorKind::Io,
            pos,
            "opening file '{}': {}",
            String::from_utf8_lossy(&path),
            crate::error::io_text(&e)
        ),
    };
    let h = hash_bytes(algo, &data).to_base16();
    Ok(ev.mk_string(h.as_bytes()))
}

fn convert_hash(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let a = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.convertHash",
    )?;
    let hash_sym = ev.sym(b"hash");
    let Some(h) = a.get(hash_sym) else {
        let mut e = Error::new(ErrorKind::Type, "attribute 'hash' missing".into(), pos);
        e.add_trace(pos, "while locating the attribute 'hash'");
        return Err(Box::new(e));
    };
    let hv = h.cell.get();
    let hash = ev.force_string_no_ctx(hv, pos, "while evaluating the attribute 'hash'")?;
    let algo_sym = ev.sym(b"hashAlgo");
    let algo = match a.get(algo_sym) {
        Some(x) => {
            let xv = x.cell.get();
            Some(parse_algo(
                ev,
                xv,
                pos,
                "while evaluating the attribute 'hashAlgo'",
            )?)
        }
        None => None,
    };
    let fmt_sym = ev.sym(b"toHashFormat");
    let Some(f) = a.get(fmt_sym) else {
        let mut e = Error::new(
            ErrorKind::Type,
            "attribute 'toHashFormat' missing".into(),
            pos,
        );
        e.add_trace(pos, "while locating the attribute 'toHashFormat'");
        return Err(Box::new(e));
    };
    let fv = f.cell.get();
    let fmt = ev.force_string_no_ctx(fv, pos, "while evaluating the attribute 'toHashFormat'")?;
    let hf = match fmt {
        b"base16" => HashFormat::Base16,
        b"nix32" => HashFormat::Nix32,
        b"base32" => {
            log::line("warning: \"base32\" is a deprecated alias for hash format \"nix32\".");
            HashFormat::Nix32
        }
        b"base64" => HashFormat::Base64,
        b"sri" => HashFormat::Sri,
        _ => bail!(
            ErrorKind::Eval,
            pos,
            "unknown hash format '{}', expect 'base16', 'base32', 'base64', or 'sri'",
            String::from_utf8_lossy(fmt)
        ),
    };
    let hs = String::from_utf8_lossy(hash);
    let h = match Hash::parse_any(&hs, algo) {
        Ok(h) => h,
        Err(e) => bail!(ErrorKind::Eval, pos, "{}", e),
    };
    let out = h.to_string_with(hf, hf == HashFormat::Sri);
    Ok(ev.mk_string(out.as_bytes()))
}

fn round(ev: &mut Evaluator, w: Word, pos: Pos, ctx: &str, op: fn(f64) -> f64) -> EvalResult<Word> {
    let w = ev.force(w)?;
    let arg_int = w.as_int();
    let value = ev.force_float(w, pos, ctx)?;
    let r = op(value);
    const INT_MIN: f64 = i64::MIN as f64;
    let res = if (INT_MIN..-INT_MIN).contains(&r) {
        r as i64
    } else if let Some(i) = arg_int {
        bail!(
            ErrorKind::Eval,
            pos,
            "Due to a bug (see https://github.com/NixOS/nix/issues/12899) the NixInt argument {} caused undefined behavior in previous Nix versions.\n\tFuture Nix versions might implement the correct behavior.",
            i
        );
    } else {
        bail!(
            ErrorKind::Eval,
            pos,
            "NixFloat argument {} is not in the range of NixInt",
            crate::print::format_g(value)
        );
    };
    if let Some(i) = arg_int
        && i != res
    {
        bail!(
            ErrorKind::Eval,
            pos,
            "Due to a bug (see https://github.com/NixOS/nix/issues/12899) a loss of precision occurred in previous Nix versions because the NixInt argument {} was rounded to {}.\n\tFuture Nix versions might implement the correct behavior.",
            i,
            res
        );
    }
    Ok(ev.mk_int(res))
}

fn floor(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    round(
        ev,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.floor",
        f64::floor,
    )
}

fn ceil(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    round(
        ev,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.ceil",
        f64::ceil,
    )
}

fn missing_attr(name: &str, ctx: &str, pos: Pos) -> Box<Error> {
    let mut e = Error::new(ErrorKind::Type, format!("attribute '{name}' missing"), pos);
    e.add_trace(pos, ctx);
    Box::new(e)
}

fn generic_closure(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let a = ev.force_attrs(
        args[0],
        Pos::NONE,
        "while evaluating the first argument passed to builtins.genericClosure",
    )?;
    let start_sym = ev.sym(b"startSet");
    let Some(start) = a.get(start_sym) else {
        return Err(missing_attr(
            "startSet",
            "in the attrset passed as argument to builtins.genericClosure",
            pos,
        ));
    };
    let sv = start.cell.get();
    let sl = ev.force_list(
        sv,
        Pos::NONE,
        "while evaluating the 'startSet' attribute passed as argument to builtins.genericClosure",
    )?;
    if sl.len == 0 {
        return Ok(Word::from_ref(sl));
    }
    let mut work: VecDeque<Word> = sl.cells().iter().map(|c| c.get()).collect();
    let op_sym = ev.sym(b"operator");
    let Some(op) = a.get(op_sym) else {
        return Err(missing_attr(
            "operator",
            "in the attrset passed as argument to builtins.genericClosure",
            pos,
        ));
    };
    let ov = op.cell.get();
    let op = ev.force_function(
        ov,
        Pos::NONE,
        "while evaluating the 'operator' attribute passed as argument to builtins.genericClosure",
    )?;
    let key_sym = ev.sym(b"key");
    // keys seen so far, ordered by `<`, like Nix's std::map<Value*, CompareValues>
    let mut keys: Vec<Word> = Vec::new();
    let mut res: Vec<Word> = Vec::new();
    while let Some(e) = work.pop_front() {
        let ea = ev.force_attrs(e, Pos::NONE, "").map_err(|mut err| {
            err.add_trace(Pos::NONE, "in genericClosure element");
            err
        })?;
        let e = Word::from_ref(ea);
        let Some(key) = ea.get(key_sym) else {
            return Err(missing_attr("key", "in genericClosure element", pos));
        };
        let kv = ev.force_cell(&key.cell)?;
        let (mut lo, mut hi) = (0usize, keys.len());
        let mut found = false;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if ev.less_than(keys[mid], kv, Pos::NONE)? {
                lo = mid + 1;
            } else if ev.less_than(kv, keys[mid], Pos::NONE)? {
                hi = mid;
            } else {
                found = true;
                break;
            }
        }
        if found {
            continue;
        }
        keys.insert(lo, kv);
        res.push(e);
        let new = ev.call1(op, e, Pos::NONE)?;
        let nl = ev.force_list(
            new,
            Pos::NONE,
            "while evaluating the return value of the `operator` passed to builtins.genericClosure",
        )?;
        for c in nl.cells() {
            let v = ev.force_cell(c)?;
            work.push_back(v);
        }
    }
    Ok(ev.mk_list(&res))
}

fn get_env(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let name = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.getEnv",
    )?;
    purity::get_env(ev, name, pos)?;
    if ev.pure() {
        return Ok(ev.mk_string(b""));
    }
    let os = <std::ffi::OsStr as crate::sys::OsStrExt>::from_bytes(name);
    ev.trace().env(name);
    let v = env::var_os(os).unwrap_or_default();
    let bytes = <std::ffi::OsStr as crate::sys::OsStrExt>::as_bytes(&v);
    Ok(ev.mk_string(bytes))
}

fn break_(ev: &mut Evaluator, args: &[Word], _pos: Pos) -> EvalResult<Word> {
    ev.force(args[0])
}
