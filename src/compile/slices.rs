//! A huge plain set's bindings, compiled in slices on several threads.
//!
//! hackage-packages.nix's 19k bindings took 37 ms of compile on one thread
//! while the other workers slept. A plain set of `PAR_MIN` bindings or more
//! is compiled in slices, each by a compiler of its own: an arena of its own
//! in the session's heap, a clone of the shared interner, copies of the
//! scope stacks. The nested sets of `a.b = …` paths stay with this compiler,
//! which owns them. A slice that fails sends the whole set back to the
//! ordered compile, so the first error is the sequential one; the warnings
//! and the captures of the enclosing lambdas come back in binding order.

use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use super::{Compiler, Def, var_uses};
use crate::compile::ir::IrRef;
use crate::eval::Pos;
use crate::lint;
use crate::runtime::arena::Arena;
use crate::session::Session;
use crate::syntax::symbol::Interner;
use crate::syntax::symbol::Sym;
use std::mem;
use std::sync::Arc;

/// Bindings in a plain set before its values are compiled in slices.
pub(super) const PAR_MIN: usize = 2048;
/// Bindings a slice gets at least.
const SLICE_MIN: usize = 256;
const SLICE_STACK: usize = 64 << 20;
/// Bytes a slice's arena maps at a time.
const SLICE_CHUNK: usize = 1 << 20;

/// Sets compiled in slices, for the tests to tell the sliced path from the
/// fallback.
#[cfg(test)]
static SLICED: AtomicUsize = AtomicUsize::new(0);

/// What one slice made: the value of every binding it compiled (`None` for
/// the nested sets it leaves), the warnings each of them produced, the
/// captures of the enclosing lambdas, and the bytes its arena gave out.
struct Slice {
    values: Vec<Option<IrRef>>,
    diags: Vec<Vec<lint::Diag>>,
    captures: Vec<bool>,
    used_home: bool,
    bytes: usize,
}

/// Every binding's value (`None` for a nested set, which the caller
/// compiles) and the warnings each produced, in binding order.
type Lowered = (Vec<Option<IrRef>>, Vec<Vec<lint::Diag>>);

/// The `Send` the compiler's borrowed context lacks: a slice only reads the
/// tree, the globals and the lint levels, and nothing writes them while the
/// slices run.
struct Shared<'c, 'a>(&'c Compiler<'a>);
// SAFETY: as above; the compiler is borrowed for the scope of the slices.
unsafe impl Sync for Shared<'_, '_> {}

impl<'a> Compiler<'a> {
    /// The values of `statics`, in order, each `Some` but a nested set's;
    /// `None` when a slice failed, and the caller compiles them in order.
    pub(super) fn lower_statics_parallel(
        &mut self,
        statics: &[(Sym, Def, Pos)],
    ) -> Option<Lowered> {
        let n = (self.threads * 4).min(statics.len() / SLICE_MIN);
        if n < 2 {
            return None;
        }
        // read by every slice, so made before them
        if self.var_uses.is_none() {
            self.var_uses = Some(Arc::new(var_uses(self.ast)));
        }
        let heap = Session::current().heap.clone();
        let threads = self.threads.min(n);
        let interners: Vec<_> = (0..threads).map(|_| self.interner.clone()).collect();
        let next = AtomicUsize::new(0);
        let out: Vec<Mutex<Option<Slice>>> = (0..n).map(|_| Mutex::new(None)).collect();
        let shared = Shared(self);
        let work = |mut interner: Interner| {
            let me = &shared;
            // one arena a thread, in small chunks: a full arena chunk is
            // 32 MB, prefaulted, for a few MB of IR
            let mut arena = Arena::in_heap_small(heap.clone(), SLICE_CHUNK);
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let lo = statics.len() * i / n;
                let hi = statics.len() * (i + 1) / n;
                *out[i].lock().unwrap() =
                    me.0.compile_slice(&statics[lo..hi], &mut arena, &mut interner);
            }
        };
        let spawned = thread::scope(|s| {
            let mut ok = true;
            let mut joins = Vec::new();
            let mut interners = interners.into_iter();
            let mine = interners.next().unwrap();
            for h in interners {
                match thread::Builder::new()
                    .stack_size(SLICE_STACK)
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
            return None;
        }
        let mut values = Vec::with_capacity(statics.len());
        let mut diags = Vec::with_capacity(statics.len());
        for o in out {
            let s = o.into_inner().unwrap()?;
            for (c, &got) in self.lambdas.iter().zip(&s.captures) {
                if got {
                    c.1.set(true);
                }
            }
            if s.used_home {
                self.used_home.set(true);
            }
            self.arena.bytes_allocated += s.bytes;
            values.extend(s.values);
            diags.extend(s.diags);
        }
        #[cfg(test)]
        SLICED.fetch_add(1, Ordering::Relaxed);
        Some((values, diags))
    }

    /// One slice, by a compiler that shares this one's tree and scopes and
    /// allocates in `arena`; `None` when a binding fails to compile.
    fn compile_slice(
        &self,
        statics: &[(Sym, Def, Pos)],
        arena: &mut Arena,
        interner: &mut Interner,
    ) -> Option<Slice> {
        let mut c = Compiler {
            ast: self.ast,
            arena,
            interner,
            globals: self.globals,
            scopes: self.scopes.clone(),
            names: self.names.clone(),
            sc_refs: Vec::new(),
            sc_withs: Vec::new(),
            sc_binds: Vec::new(),
            lambdas: self
                .lambdas
                .iter()
                .map(|(f, _)| (*f, Cell::new(false)))
                .collect(),
            var_uses: self.var_uses.clone(),
            base_dir: self.base_dir.clone(),
            base: self.base,
            home: self.home.clone(),
            used_home: Cell::new(false),
            lints: self.lints,
            diags: Vec::new(),
            sym_builtins: self.sym_builtins,
            sym_rev_count: self.sym_rev_count,
            sym_cur_pos: self.sym_cur_pos,
            sym_overrides: self.sym_overrides,
            sym_body: self.sym_body,
            threads: 1,
        };
        let start = c.arena.bytes_allocated;
        let mut values = Vec::with_capacity(statics.len());
        let mut diags = Vec::with_capacity(statics.len());
        for (sym, def, bpos) in statics {
            let value = match def {
                Def::Expr(e) => Some(c.value_expr(*e, Some(*sym)).ok()?),
                Def::Inherit(var) => Some(c.var_ir(*var, *bpos).ok()?),
                Def::Nested(_) | Def::InheritFrom(..) => None,
            };
            values.push(value);
            diags.push(mem::take(&mut c.diags));
        }
        Some(Slice {
            values,
            diags,
            captures: c.lambdas.iter().map(|(_, got)| got.get()).collect(),
            used_home: c.used_home.get(),
            bytes: c.arena.bytes_allocated - start,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::SLICED;
    use crate::compile::{Compiler, cache};
    use crate::eval::EvalConfig;
    use crate::lint::allow::Allows;
    use crate::syntax::{self, ast::FileId};
    use std::env;
    use std::fs;

    /// The IR cache entry of `src` compiled with `threads`: the same bytes
    /// for the same IR.
    fn ir_bytes(src: &str, threads: usize) -> Vec<u8> {
        let mut ev = EvalConfig::new().build();
        let ast = syntax::parse(src.as_bytes(), FileId(0), &mut ev.interner).unwrap();
        let mut c = Compiler::new(
            &ast,
            &mut ev.arena,
            &mut ev.interner,
            &ev.globals,
            0,
            b"/x",
            None,
        );
        c.set_threads(threads);
        let ir = c.compile_root(None).unwrap_or_else(|e| panic!("{}", e.msg));
        drop(c);
        cache::serialize(&ev, ir, 0, &[], &Allows::default()).unwrap()
    }

    fn assert_same(src: &str) {
        let before = SLICED.load(Ordering::Relaxed);
        let par = ir_bytes(src, 8);
        assert!(
            SLICED.load(Ordering::Relaxed) > before,
            "the set was not sliced"
        );
        assert!(par == ir_bytes(src, 1), "the sliced IR differs");
    }

    #[test]
    fn a_huge_plain_set_compiles_in_slices_to_the_sequential_ir() {
        // only the sliced bindings read `lib` and `fetchurl`, so the
        // captures of the enclosing lambdas come from the slices alone
        let mut s = String::from("{ lib, fetchurl }:\nself: {\n  inherit fetchurl;\n");
        for i in 0..3000 {
            s.push_str(&format!(
                "  p{i} = {{ a ? {i} }}: lib.f self.p{j} a;\n  q{i} = x: x + {i};\n",
                j = (i * 7) % 3000
            ));
            if i % 50 == 0 {
                s.push_str(&format!(
                    "  inherit (self) r{i};\n  n{i}.a = self.q{i};\n  n{i}.b = let y = {i}; in y;\n"
                ));
            }
        }
        s.push_str("}\n");
        assert_same(&s);
    }

    /// `IETS_SLICE_FILE=…/hackage-packages.nix cargo test -- --ignored
    /// a_real_huge_set --nocapture`: a real file compiles to the sequential
    /// IR, and says whether it was sliced (a `rec` set or a `let` is not).
    #[test]
    #[ignore]
    fn a_real_huge_set_compiles_in_slices_to_the_sequential_ir() {
        let path = env::var("IETS_SLICE_FILE").expect("IETS_SLICE_FILE names a file");
        let src = String::from_utf8(fs::read(path).unwrap()).unwrap();
        let before = SLICED.load(Ordering::Relaxed);
        let par = ir_bytes(&src, 8);
        eprintln!("sliced: {}", SLICED.load(Ordering::Relaxed) > before);
        assert!(par == ir_bytes(&src, 1), "the sliced IR differs");
    }
}
