//! String primops.

use super::Registry;
use crate::eval::{self, ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::parallel;
use crate::runtime::{Ctx, Value, Word, ctx_to_arena};

use std::borrow::Cow;
use std::cmp::Ordering;

pub fn register(r: &mut Registry) {
    r.op("__stringLength", 1, string_length);
    r.op("__substring", 3, substring);
    r.op("__concatStringsSep", 2, concat_strings_sep);
    r.op("__splitVersion", 1, split_version);
    r.op("__compareVersions", 2, compare_versions);
    r.op("__parseDrvName", 1, parse_drv_name);
    r.op("baseNameOf", 1, base_name_of);
    r.op("dirOf", 1, dir_of);
}

fn string_length(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let mut ctx = Vec::new();
    let s = ev.coerce_to_string(
        args[0],
        &mut ctx,
        pos,
        "while evaluating the argument passed to builtins.stringLength",
        false,
        true,
        true,
    )?;
    Ok(Word::imm_int(s.len() as i64))
}

fn substring(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let start = ev.force_int(
        args[0],
        pos,
        "while evaluating the first argument (the start offset) passed to builtins.substring",
    )?;
    if start < 0 {
        bail!(
            ErrorKind::Eval,
            pos,
            "negative start position in 'substring'"
        );
    }
    let len = ev.force_int(
        args[1],
        pos,
        "while evaluating the second argument (the substring length) passed to builtins.substring",
    )?;
    if len == 0 {
        let w = ev.force(args[2])?;
        if let Some(s) = w.as_string() {
            return Ok(ev.arena.mk_string_static(b"", s.ctx));
        }
    }
    let mut ctx = Vec::new();
    let s = ev.coerce_to_string(
        args[2],
        &mut ctx,
        pos,
        "while evaluating the third argument (the string) passed to builtins.substring",
        false,
        true,
        true,
    )?;
    let start = (start as usize).min(s.len());
    let end = if len < 0 {
        s.len()
    } else {
        (start + len as usize).min(s.len())
    };
    let ctx = ctx_to_arena(&mut ev.arena, ctx);
    let i = match s {
        Cow::Borrowed(b) => ev.arena.intern_static(&b[start..end]),
        Cow::Owned(b) => ev.arena.intern_bytes(&b[start..end]),
    };
    Ok(ev.arena.mk_string_over(i, ctx))
}

fn concat_strings_sep(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let sep = ev.force_string(
        args[0],
        pos,
        "while evaluating the first argument (the separator string) passed to builtins.concatStringsSep",
    )?;
    let l = ev.force_list(
        args[1],
        pos,
        "while evaluating the second argument (the list of strings to concat) passed to builtins.concatStringsSep",
    )?;
    parallel::fanout_list_coerce(ev, l);
    let mut ctx: Vec<Ctx> = sep.ctx.to_vec();
    let mut out: Vec<u8> = Vec::new();
    for (i, c) in l.cells().iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(sep.bytes);
        }
        let s = ev.coerce_to_string(
            c.get(),
            &mut ctx,
            pos,
            "while evaluating one element of the list of strings to concat passed to builtins.concatStringsSep",
            false,
            true,
            true,
        )?;
        out.extend_from_slice(&s);
    }
    let ctx = ctx_to_arena(&mut ev.arena, ctx);
    Ok(ev.arena.mk_string_shared(&out, ctx))
}

/// Nix version component splitting (`names.cc: nextComponent`).
fn next_component<'a>(s: &'a [u8], p: &mut usize) -> &'a [u8] {
    while *p < s.len() && (s[*p] == b'.' || s[*p] == b'-') {
        *p += 1;
    }
    if *p >= s.len() {
        return b"";
    }
    let start = *p;
    if s[*p].is_ascii_digit() {
        while *p < s.len() && s[*p].is_ascii_digit() {
            *p += 1;
        }
    } else {
        while *p < s.len() && !s[*p].is_ascii_digit() && s[*p] != b'.' && s[*p] != b'-' {
            *p += 1;
        }
    }
    &s[start..*p]
}

/// `names.cc:componentsLT` reads a component with `string2Int<int>`, so a
/// value above 2147483647 is not a number and orders as text.
fn parse_int(c: &[u8]) -> Option<i32> {
    if c.is_empty() {
        return None;
    }
    let mut n: i32 = 0;
    for &b in c {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as i32)?;
    }
    Some(n)
}

fn components_lt(c1: &[u8], c2: &[u8]) -> bool {
    let n1 = parse_int(c1);
    let n2 = parse_int(c2);
    match (n1, n2) {
        (Some(a), Some(b)) => a < b,
        (None, Some(_)) if c1.is_empty() => true,
        _ => {
            if c1 == b"pre" && c2 != b"pre" {
                true
            } else if c2 == b"pre" {
                false
            } else if n2.is_some() {
                true
            } else if n1.is_some() {
                false
            } else {
                c1 < c2
            }
        }
    }
}

pub fn compare_versions_bytes(a: &[u8], b: &[u8]) -> Ordering {
    let (mut p1, mut p2) = (0usize, 0usize);
    loop {
        if p1 >= a.len() && p2 >= b.len() {
            return Ordering::Equal;
        }
        let c1 = next_component(a, &mut p1);
        let c2 = next_component(b, &mut p2);
        if components_lt(c1, c2) {
            return Ordering::Less;
        }
        if components_lt(c2, c1) {
            return Ordering::Greater;
        }
        if c1.is_empty() && c2.is_empty() {
            return Ordering::Equal;
        }
    }
}

fn split_version(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let s = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.splitVersion",
    )?;
    let mut p = 0;
    let mut items = Vec::new();
    loop {
        let c = next_component(s, &mut p);
        if c.is_empty() {
            break;
        }
        items.push(ev.arena.mk_string(c));
    }
    Ok(ev.mk_list(&items))
}

fn compare_versions(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let a = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.compareVersions",
    )?;
    let b = ev.force_string_no_ctx(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.compareVersions",
    )?;
    Ok(Word::imm_int(match compare_versions_bytes(a, b) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }))
}

pub fn parse_drv_name_bytes(s: &[u8]) -> (&[u8], &[u8]) {
    for i in 0..s.len() {
        if s[i] == b'-' && i + 1 < s.len() && !s[i + 1].is_ascii_alphabetic() {
            return (&s[..i], &s[i + 1..]);
        }
    }
    (s, b"")
}

fn parse_drv_name(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let s = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.parseDrvName",
    )?;
    let (name, version) = parse_drv_name_bytes(s);
    let n = ev.arena.mk_string(name);
    let v = ev.arena.mk_string(version);
    let mut entries = vec![(ev.s.name, Pos::NONE, n), (ev.s.version, Pos::NONE, v)];
    ev.mk_attrs(&mut entries, pos)
}

fn base_name_of(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let mut ctx = Vec::new();
    let s = ev.coerce_to_string(
        args[0],
        &mut ctx,
        pos,
        "while evaluating the first argument passed to builtins.baseNameOf",
        false,
        false,
        true,
    )?;
    let mut end = s.len();
    if end > 0 && s[end - 1] == b'/' {
        end -= 1;
    }
    let start = s[..end]
        .iter()
        .rposition(|&b| b == b'/')
        .map_or(0, |i| i + 1);
    let out = &s[start..end];
    let ctx = ctx_to_arena(&mut ev.arena, ctx);
    Ok(ev.arena.mk_string_shared(out, ctx))
}

fn dir_of(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let w = ev.force(args[0])?;
    if let Value::Path(p) = w.view() {
        let d = eval::dir_of(p.path);
        return Ok(ev.arena.mk_path(d));
    }
    let mut ctx = Vec::new();
    let s = ev.coerce_to_string(
        w,
        &mut ctx,
        pos,
        "while evaluating the first argument passed to 'builtins.dirOf'",
        false,
        false,
        true,
    )?;
    let out: &[u8] = match s.iter().rposition(|&b| b == b'/') {
        None => b".",
        Some(0) => b"/",
        Some(i) => &s[..i],
    };
    let ctx = ctx_to_arena(&mut ev.arena, ctx);
    Ok(ev.arena.mk_string_shared(out, ctx))
}
