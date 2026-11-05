//! A huge set's bindings, parsed in chunks on several threads.
//!
//! hackage-packages.nix is 16.6 MB, 42 % of the source nixos-server loads,
//! and its parse ran alone on one thread while the other workers slept. A
//! set with more than `CHUNKED_MIN` of source left is cut at lines indented
//! like its first binding after a line that ends in `;`. Each chunk is
//! parsed on its own thread by a parser of its own, then appended to the
//! tree relocated. The cut is a guess: it holds only when every chunk
//! parses as complete bindings to its end exactly. If any does not, nothing
//! is kept and the parse goes on sequentially, so the tree and the errors
//! are the sequential ones.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use super::Parser;
use crate::syntax::ast::Ast;
use crate::syntax::ast::{
    AttrElem, Bind, BindKind, Expr, ExprId, Formal, Note, Param, PathExpr, Range, StrPart,
};
use crate::syntax::lexer::Tok;
use crate::syntax::symbol::Interner;
use crate::sys;
use std::mem;

/// Source left in a set before its bindings are parsed in chunks.
pub(super) const CHUNKED_MIN: usize = 512 << 10;
const MAX_CHUNKS: usize = 64;
/// The last chunk's end stays this far from the end of the file, where the
/// set may close.
const CHUNK_TAIL: usize = 32 << 10;
/// A chunk smaller than this is not worth a thread.
const STEP_MIN: usize = 64 << 10;
/// Nesting a chunk may add before it gives up; the sequential parse, on the
/// evaluator's large stack, takes over then.
const CHUNK_DEPTH: u32 = 4096;
const CHUNK_STACK: usize = 64 << 20;

/// Sets parsed in chunks, for the tests to tell the chunked path from the
/// fallback.
#[cfg(test)]
static CHUNKED: AtomicUsize = AtomicUsize::new(0);

/// What a chunk's parser leaves: its tables, its top-level bindings, its
/// `''` strings and its `# iets:` comments, all in its own numbering.
struct Parsed {
    ast: Ast,
    binds: Vec<Bind>,
    ind_strs: Vec<(ExprId, bool)>,
    allows: Vec<(usize, usize)>,
    last_end: u32,
}

impl<'a> Parser<'a> {
    /// The bindings of the set being parsed, from `start` on, parsed in
    /// chunks on `self.threads` threads and appended as the sequential
    /// parse would have made them; this parser then goes on from the end
    /// of the last chunk. Leaves the parser as it was when any chunk fails.
    pub(super) fn parallel_binds(&mut self, start: usize) {
        let want = (self.threads * 3).clamp(2, MAX_CHUNKS);
        let Some(bounds) = chunk_bounds(self.src, start, want) else {
            return;
        };
        let n = bounds.len() - 1;
        let threads = self.threads.min(n);
        let src = self.src;
        let depth = self.depth;
        let next = AtomicUsize::new(0);
        let out: Vec<Mutex<Option<Parsed>>> = (0..n).map(|_| Mutex::new(None)).collect();
        let work = |mut interner: Interner| {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let p = parse_chunk(src, bounds[i], bounds[i + 1], &mut interner, depth);
                *out[i].lock().unwrap() = p;
            }
        };
        let handles: Vec<Interner> = (0..threads).map(|_| self.interner.clone()).collect();
        let spawned = thread::scope(|s| {
            let mut ok = true;
            let mut joins = Vec::new();
            let mut handles = handles.into_iter();
            let mine = handles.next().unwrap();
            for h in handles {
                match thread::Builder::new()
                    .stack_size(CHUNK_STACK)
                    .spawn_scoped(s, || work(h))
                {
                    Ok(j) => joins.push(j),
                    Err(_) => ok = false,
                }
            }
            work(mine);
            for j in joins {
                ok &= j.join().is_ok();
            }
            ok
        });
        if !spawned {
            return;
        }
        let mut parsed = Vec::with_capacity(n);
        for o in out {
            match o.into_inner().unwrap() {
                Some(p) => parsed.push(p),
                None => return,
            }
        }
        // every table grows once, by the chunks' sizes, so no append
        // reallocates; each chunk's tables go back to the OS as soon as they
        // are appended. Freed and left to the purge delay, they stayed
        // resident beside the whole tree: hackage-packages.nix, 8 workers,
        // a haskellPackages eval 536 -> 414 MB peak, 168 -> 131 ms.
        self.reserve(&parsed);
        let mut last_end = self.last_end;
        let mut allows = Vec::new();
        for mut p in parsed {
            self.append(&p);
            last_end = p.last_end;
            allows.append(&mut p.allows);
            drop(p);
            sys::release_freed();
        }
        self.tok = None;
        self.next = None;
        self.lx.resume_after(bounds[n], allows);
        self.last_end = last_end;
        #[cfg(test)]
        CHUNKED.fetch_add(1, Ordering::Relaxed);
    }

    fn reserve(&mut self, parsed: &[Parsed]) {
        let sum = |f: fn(&Parsed) -> usize| parsed.iter().map(f).sum::<usize>();
        let a = &mut self.ast;
        a.exprs.reserve(sum(|p| p.ast.exprs.len()));
        a.spans.reserve(sum(|p| p.ast.spans.len()));
        a.ids.reserve(sum(|p| p.ast.ids.len()));
        a.elems.reserve(sum(|p| p.ast.elems.len()));
        a.parts.reserve(sum(|p| p.ast.parts.len()));
        a.bytes.reserve(sum(|p| p.ast.bytes.len()));
        a.binds.reserve(sum(|p| p.ast.binds.len()));
        a.names.reserve(sum(|p| p.ast.names.len()));
        a.formals.reserve(sum(|p| p.ast.formals.len()));
        a.notes.reserve(sum(|p| p.ast.notes.len()));
        self.sc_binds.reserve(sum(|p| p.binds.len()));
        self.ind_strs.reserve(sum(|p| p.ind_strs.len()));
    }

    /// Appends a chunk's entries to this parser's tables, every node and
    /// range shifted by where they land.
    fn append(&mut self, p: &Parsed) {
        let a = &mut self.ast;
        let o = Offsets {
            exprs: a.exprs.len() as u32,
            ids: a.ids.len() as u32,
            elems: a.elems.len() as u32,
            parts: a.parts.len() as u32,
            bytes: a.bytes.len() as u32,
            binds: a.binds.len() as u32,
            names: a.names.len() as u32,
            formals: a.formals.len() as u32,
        };
        let c = &p.ast;
        a.exprs.extend(c.exprs.iter().map(|e| o.expr(*e)));
        a.spans.extend_from_slice(&c.spans);
        a.ids.extend(c.ids.iter().map(|&e| o.id(e)));
        a.elems.extend(c.elems.iter().map(|&e| o.elem(e)));
        a.parts.extend(c.parts.iter().map(|&x| match x {
            StrPart::Lit(r) => StrPart::Lit(o.range(r, o.bytes)),
            StrPart::Interp(e) => StrPart::Interp(o.id(e)),
        }));
        a.bytes.extend_from_slice(&c.bytes);
        a.binds.extend(c.binds.iter().map(|&b| o.bind(b)));
        a.names.extend_from_slice(&c.names);
        a.formals.extend(c.formals.iter().map(|&f| Formal {
            default: f.default.map(|e| o.id(e)),
            ..f
        }));
        a.notes.extend(c.notes.iter().map(|&n| match n {
            Note::Url(e) => Note::Url(o.id(e)),
            Note::UnknownAllow { at, msg } => Note::UnknownAllow {
                at,
                msg: o.range(msg, o.bytes),
            },
        }));
        self.sc_binds.extend(p.binds.iter().map(|&b| o.bind(b)));
        self.ind_strs
            .extend(p.ind_strs.iter().map(|&(e, concat)| (o.id(e), concat)));
    }
}

/// Where a chunk's entries land in each table.
struct Offsets {
    exprs: u32,
    ids: u32,
    elems: u32,
    parts: u32,
    bytes: u32,
    binds: u32,
    names: u32,
    formals: u32,
}

impl Offsets {
    #[inline]
    fn id(&self, e: ExprId) -> ExprId {
        ExprId(e.0 + self.exprs)
    }

    #[inline]
    fn range(&self, r: Range, by: u32) -> Range {
        Range {
            start: r.start + by,
            len: r.len,
        }
    }

    fn elem(&self, e: AttrElem) -> AttrElem {
        match e {
            AttrElem::Static(_) => e,
            AttrElem::Dynamic(x) => AttrElem::Dynamic(self.id(x)),
        }
    }

    fn bind(&self, b: Bind) -> Bind {
        let kind = match b.kind {
            BindKind::Value { path, value } => BindKind::Value {
                path: self.range(path, self.elems),
                value: self.id(value),
            },
            BindKind::Inherit { from, names } => BindKind::Inherit {
                from: from.map(|e| self.id(e)),
                names: self.range(names, self.names),
            },
        };
        Bind { span: b.span, kind }
    }

    fn expr(&self, e: Expr) -> Expr {
        match e {
            Expr::Int(_) | Expr::Float(_) | Expr::Var(_) => e,
            Expr::Str(r) => Expr::Str(self.range(r, self.parts)),
            Expr::Path(p) => Expr::Path(PathExpr {
                kind: p.kind,
                parts: self.range(p.parts, self.parts),
            }),
            Expr::Select {
                subject,
                path,
                default,
            } => Expr::Select {
                subject: self.id(subject),
                path: self.range(path, self.elems),
                default: default.map(|d| self.id(d)),
            },
            Expr::HasAttr { subject, path } => Expr::HasAttr {
                subject: self.id(subject),
                path: self.range(path, self.elems),
            },
            Expr::Attrs { rec, binds } => Expr::Attrs {
                rec,
                binds: self.range(binds, self.binds),
            },
            Expr::List(r) => Expr::List(self.range(r, self.ids)),
            Expr::Lambda { param, body } => Expr::Lambda {
                param: match param {
                    Param::Ident(_) => param,
                    Param::Formals {
                        bind,
                        formals,
                        ellipsis,
                    } => Param::Formals {
                        bind,
                        formals: self.range(formals, self.formals),
                        ellipsis,
                    },
                },
                body: self.id(body),
            },
            Expr::Apply { func, args } => Expr::Apply {
                func: self.id(func),
                args: self.range(args, self.ids),
            },
            Expr::Let { binds, body } => Expr::Let {
                binds: self.range(binds, self.binds),
                body: self.id(body),
            },
            Expr::With { scope, body } => Expr::With {
                scope: self.id(scope),
                body: self.id(body),
            },
            Expr::If { cond, then, else_ } => Expr::If {
                cond: self.id(cond),
                then: self.id(then),
                else_: self.id(else_),
            },
            Expr::Assert { cond, body } => Expr::Assert {
                cond: self.id(cond),
                body: self.id(body),
            },
            Expr::Unary { op, expr } => Expr::Unary {
                op,
                expr: self.id(expr),
            },
            Expr::Binary { op, lhs, rhs } => Expr::Binary {
                op,
                lhs: self.id(lhs),
                rhs: self.id(rhs),
            },
        }
    }
}

/// The chunk `src[start..end]`'s bindings, as a parser of its own reads
/// them; `None` unless they run to its end exactly.
fn parse_chunk(
    src: &[u8],
    start: usize,
    end: usize,
    interner: &mut Interner,
    depth: u32,
) -> Option<Parsed> {
    let mut p = Parser::chunk(&src[..end], interner, start, end - start);
    p.depth = depth;
    p.max_depth = depth + CHUNK_DEPTH;
    p.parse_binds_until(Tok::Eof).ok()?;
    Some(Parsed {
        last_end: p.last_end,
        binds: mem::take(&mut p.sc_binds),
        ind_strs: mem::take(&mut p.ind_strs),
        allows: p.lx.take_raw_allows(),
        ast: mem::take(&mut p.ast),
    })
}

/// Chunk boundaries for the bindings from `start` on, about evenly spaced:
/// `n + 1` offsets for `n` chunks, the last the end of the last chunk.
/// `None` when the source offers too few.
fn chunk_bounds(src: &[u8], start: usize, want: usize) -> Option<Vec<usize>> {
    // The first binding's indentation; it must stand on a line of its own.
    let mut ls = start;
    while ls > 0 && src[ls - 1] == b' ' {
        ls -= 1;
    }
    if ls > 0 && src[ls - 1] != b'\n' {
        return None;
    }
    let indent = start - ls;
    if src.len() < CHUNK_TAIL + start {
        return None;
    }
    let end = src.len() - CHUNK_TAIL;
    let step = (end - start) / want;
    if step < STEP_MIN {
        return None;
    }
    let mut out = vec![start];
    let mut target = start + step;
    while out.len() < want && target < end {
        let Some(b) = next_boundary(src, target, end, indent) else {
            break;
        };
        out.push(b);
        target = b + step;
    }
    // two chunks at least: the last boundary is where the last one ends
    (out.len() >= 3).then_some(out)
}

/// The first line start at or after `from` and before `end` indented by
/// exactly `indent` spaces and then a name, after a line ending in `;`
/// (blank lines between).
fn next_boundary(src: &[u8], from: usize, end: usize, indent: usize) -> Option<usize> {
    let mut i = from;
    while let Some(off) = src[i..end].iter().position(|&b| b == b'\n') {
        let nl = i + off;
        i = nl + 1;
        let b = nl + 1;
        if b + indent >= end {
            return None;
        }
        let line = &src[b..];
        if !line[..indent].iter().all(|&c| c == b' ') {
            continue;
        }
        let c = line[indent];
        if !(c.is_ascii_alphabetic() || c == b'_' || c == b'"') {
            continue;
        }
        let mut k = nl;
        while k > 0 && matches!(src[k - 1], b' ' | b'\t' | b'\r' | b'\n') {
            k -= 1;
        }
        if k > 0 && src[k - 1] == b';' {
            return Some(b);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::CHUNKED;
    use crate::syntax::ast::{Ast, FileId};
    use crate::syntax::dump::dump;
    use crate::syntax::symbol::Interner;
    use crate::syntax::{ParseError, parse, parse_threads};
    use std::env;
    use std::fs;
    use std::sync::atomic::Ordering;

    /// A set of `n` bindings with every node kind, `# iets:` comments and
    /// `''` strings, as a huge package set has them; `extra` goes in the
    /// middle.
    fn huge(n: usize, extra: &str) -> String {
        let mut s = String::from("{ lib, fetchurl }:\nself: {\n");
        for i in 0..n {
            if i == n / 2 {
                s.push_str(extra);
            }
            if i % 97 == 0 {
                s.push_str("  # iets: allow no-such-lint\n");
            }
            s.push_str(&format!(
                "  p{i} = self.callPackage ({{ mkDerivation, base ? null, ... }}@args: mkDerivation {{\n    pname = \"p{i}\"; version = \"1.${{toString {i}}}\";\n    src = ./x/p{i}; homepage = http://example.org/{i};\n    description = ''\n      line ${{lib.x}} {i}\n        indented\n    '';\n    deps = [ base self.\"q-{i}\" (-{i}) 1.5e3 ];\n    inherit (lib) foo bar; inherit base;\n    ok = !(args ? base) && (args.base or null) == null;\n    v = let a = {i}; in if a > 2 then with lib; a else assert true; a;\n  }}) {{}};\n"
            ));
        }
        s.push_str("}\n");
        s
    }

    type Parsed = Result<(Ast, Interner), ParseError>;

    fn both(src: &str) -> (Parsed, Parsed) {
        let mut i1 = Interner::new();
        let a = parse(src.as_bytes(), FileId(0), &mut i1).map(|a| (a, i1));
        let mut i2 = Interner::new();
        let b = parse_threads(src.as_bytes(), FileId(0), &mut i2, 4).map(|a| (a, i2));
        (a, b)
    }

    fn assert_same(src: &str) {
        let (a, b) = both(src);
        let ((a, ia), (b, ib)) = (a.unwrap(), b.unwrap());
        assert_eq!(dump(&a, &ia), dump(&b, &ib));
        assert_eq!(a.spans, b.spans);
        assert_eq!(a.ids, b.ids);
        assert_eq!(format!("{:?}", a.parts), format!("{:?}", b.parts));
        assert_eq!(a.bytes, b.bytes);
        let spans = |x: &Ast| x.binds.iter().map(|b| b.span).collect::<Vec<_>>();
        assert_eq!(spans(&a), spans(&b));
        assert_eq!(format!("{:?}", a.notes), format!("{:?}", b.notes));
        assert_eq!(a.allows, b.allows);
        assert_eq!(a.root, b.root);
    }

    #[test]
    fn a_huge_set_parses_in_chunks_to_the_sequential_tree() {
        let src = huge(4000, "");
        assert!(src.len() > 1 << 20);
        let before = CHUNKED.load(Ordering::Relaxed);
        assert_same(&src);
        assert!(super::CHUNKED.load(std::sync::atomic::Ordering::Relaxed) > before);
    }

    #[test]
    fn a_guessed_cut_inside_a_string_falls_back_to_the_sequential_parse() {
        // a line in a '' string that looks like the start of a binding
        let decoy: String = (0..400)
            .map(|i| format!("  s{i} = ''\n    x;\n  y{i} = 1;\n  '';\n"))
            .collect();
        assert_same(&huge(4000, &decoy));
    }

    /// `IETS_CHUNK_FILE=…/hackage-packages.nix cargo test -- --ignored
    /// a_real_huge_file --nocapture`: a real file parses to the sequential
    /// tree, and says whether it was chunked (a huge `let`, or a set that
    /// offers no cut, is not).
    #[test]
    #[ignore]
    fn a_real_huge_file_parses_in_chunks_to_the_sequential_tree() {
        let path = env::var("IETS_CHUNK_FILE").expect("IETS_CHUNK_FILE names a file");
        let src = String::from_utf8(fs::read(path).unwrap()).unwrap();
        let before = CHUNKED.load(Ordering::Relaxed);
        assert_same(&src);
        let chunked = CHUNKED.load(Ordering::Relaxed) > before;
        eprintln!("chunked: {chunked}");
    }

    #[test]
    fn an_error_in_a_huge_set_is_the_sequential_error() {
        let src = huge(4000, "  broken = { a = ; };\n");
        let (a, b) = both(&src);
        let (a, b) = (a.err().unwrap(), b.err().unwrap());
        assert_eq!(a, b);
    }
}
