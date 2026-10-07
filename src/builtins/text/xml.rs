//! `builtins.toXML`, byte-exact with `value-to-xml.cc` and `xml-writer.cc`.

use std::collections::HashSet;
use std::iter;

use crate::compile::ir::ParamIr;
use crate::eval::{EvalResult, Evaluator, Pos};
use crate::print;
use crate::runtime::{AttrsObj, Ctx, Value, Word, ctx_to_arena};

pub fn to_xml(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let mut out: Vec<u8> = Vec::new();
    let mut ctx: Vec<Ctx> = Vec::new();
    let mut drvs_seen: HashSet<&[u8]> = HashSet::new();
    out.extend_from_slice(b"<?xml version='1.0' encoding='utf-8'?>\n<expr>\n");
    print_xml(ev, args[0], &mut out, 1, &mut ctx, &mut drvs_seen, pos)?;
    out.extend_from_slice(b"</expr>\n");
    let ctx = ctx_to_arena(&mut ev.arena, ctx);
    Ok(ev.arena.mk_string_shared(&out, ctx))
}

fn indent(out: &mut Vec<u8>, depth: usize) {
    out.extend(iter::repeat_n(b' ', depth * 2));
}

fn attr_value(out: &mut Vec<u8>, s: &[u8]) {
    for &c in s {
        match c {
            b'"' => out.extend_from_slice(b"&quot;"),
            b'<' => out.extend_from_slice(b"&lt;"),
            b'>' => out.extend_from_slice(b"&gt;"),
            b'&' => out.extend_from_slice(b"&amp;"),
            b'\n' => out.extend_from_slice(b"&#xA;"),
            _ => out.push(c),
        }
    }
}

fn write_attrs(out: &mut Vec<u8>, attrs: &[(&str, &[u8])]) {
    for (k, v) in attrs {
        out.push(b' ');
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(b"=\"");
        attr_value(out, v);
        out.push(b'"');
    }
}

fn empty_element(out: &mut Vec<u8>, depth: usize, name: &str, attrs: &[(&str, &[u8])]) {
    indent(out, depth);
    out.push(b'<');
    out.extend_from_slice(name.as_bytes());
    write_attrs(out, attrs);
    out.extend_from_slice(b" />\n");
}

fn open_element(out: &mut Vec<u8>, depth: usize, name: &str, attrs: &[(&str, &[u8])]) {
    indent(out, depth);
    out.push(b'<');
    out.extend_from_slice(name.as_bytes());
    write_attrs(out, attrs);
    out.extend_from_slice(b">\n");
}

fn close_element(out: &mut Vec<u8>, depth: usize, name: &str) {
    indent(out, depth);
    out.extend_from_slice(b"</");
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b">\n");
}

fn show_attrs(
    ev: &mut Evaluator,
    a: &'static AttrsObj,
    out: &mut Vec<u8>,
    depth: usize,
    ctx: &mut Vec<Ctx>,
    drvs_seen: &mut HashSet<&'static [u8]>,
) -> EvalResult<()> {
    for attr in print::lexicographic_order(ev, a) {
        let name = ev.interner.name(attr.sym);
        open_element(out, depth, "attr", &[("name", name)]);
        print_xml(
            ev,
            attr.cell.get(),
            out,
            depth + 1,
            ctx,
            drvs_seen,
            attr.pos,
        )?;
        close_element(out, depth, "attr");
    }
    Ok(())
}

fn print_xml(
    ev: &mut Evaluator,
    w: Word,
    out: &mut Vec<u8>,
    depth: usize,
    ctx: &mut Vec<Ctx>,
    drvs_seen: &mut HashSet<&'static [u8]>,
    pos: Pos,
) -> EvalResult<()> {
    ev.enter_call(pos)?;
    let r = print_xml_inner(ev, w, out, depth, ctx, drvs_seen, pos);
    ev.call_depth -= 1;
    r
}

fn print_xml_inner(
    ev: &mut Evaluator,
    w: Word,
    out: &mut Vec<u8>,
    depth: usize,
    ctx: &mut Vec<Ctx>,
    drvs_seen: &mut HashSet<&'static [u8]>,
    pos: Pos,
) -> EvalResult<()> {
    let w = ev.force(w)?;
    match w.view() {
        Value::Int(i) => empty_element(out, depth, "int", &[("value", i.to_string().as_bytes())]),
        Value::Bool(b) => empty_element(
            out,
            depth,
            "bool",
            &[("value", if b { b"true" } else { b"false" })],
        ),
        Value::String(s) => {
            ctx.extend_from_slice(s.ctx);
            empty_element(out, depth, "string", &[("value", s.bytes)]);
        }
        Value::Path(p) => empty_element(out, depth, "path", &[("value", p.path)]),
        Value::Null => empty_element(out, depth, "null", &[]),
        Value::Float(f) => empty_element(
            out,
            depth,
            "float",
            &[("value", print::format_g(f).as_bytes())],
        ),
        Value::Attrs(a) => {
            if ev.is_derivation(w)? {
                let mut xml_attrs: Vec<(&str, &[u8])> = Vec::new();
                let mut drv_path: &'static [u8] = b"";
                if let Some(d) = a.get(ev.s.drv_path) {
                    let v = ev.force_cell(&d.cell)?;
                    if let Some(s) = v.as_string() {
                        drv_path = s.bytes;
                        xml_attrs.push(("drvPath", s.bytes));
                    }
                }
                if let Some(o) = a.get(ev.s.out_path) {
                    let v = ev.force_cell(&o.cell)?;
                    if let Some(s) = v.as_string() {
                        xml_attrs.push(("outPath", s.bytes));
                    }
                }
                open_element(out, depth, "derivation", &xml_attrs);
                if !drv_path.is_empty() && drvs_seen.insert(drv_path) {
                    show_attrs(ev, a, out, depth + 1, ctx, drvs_seen)?;
                } else {
                    empty_element(out, depth + 1, "repeated", &[]);
                }
                close_element(out, depth, "derivation");
            } else {
                open_element(out, depth, "attrs", &[]);
                show_attrs(ev, a, out, depth + 1, ctx, drvs_seen)?;
                close_element(out, depth, "attrs");
            }
        }
        Value::List(l) => {
            open_element(out, depth, "list", &[]);
            for c in l.cells() {
                print_xml(ev, c.get(), out, depth + 1, ctx, drvs_seen, pos)?;
            }
            close_element(out, depth, "list");
        }
        Value::Lambda(l) => {
            open_element(out, depth, "function", &[]);
            match &l.lam.param {
                ParamIr::Formals {
                    formals, ellipsis, ..
                } => {
                    let mut xml_attrs: Vec<(&str, &[u8])> = Vec::new();
                    if *ellipsis {
                        xml_attrs.push(("ellipsis", b"1"));
                    }
                    if let Some(arg) = l.lam.arg {
                        xml_attrs.push(("name", ev.interner.name(arg)));
                    }
                    open_element(out, depth + 1, "attrspat", &xml_attrs);
                    let mut names: Vec<&[u8]> =
                        formals.iter().map(|f| ev.interner.name(f.sym)).collect();
                    names.sort();
                    for n in names {
                        empty_element(out, depth + 2, "attr", &[("name", n)]);
                    }
                    close_element(out, depth + 1, "attrspat");
                }
                ParamIr::Ident => {
                    let name = l.lam.arg.map(|a| ev.interner.name(a)).unwrap_or(b"");
                    empty_element(out, depth + 1, "varpat", &[("name", name)]);
                }
            }
            close_element(out, depth, "function");
        }
        Value::PrimOp(_) | Value::PrimOpApp(_) => empty_element(out, depth, "unevaluated", &[]),
    }
    Ok(())
}
