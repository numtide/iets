//! String coercion, equality, and ordering, mirroring `EvalState::coerceToString`,
//! `eqValues`, and `CompareValues`.

use std::borrow::Cow;

use super::{Error, ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::builtins::store_hooks;
use crate::lint::{LintId, coerce};
use crate::runtime::{Cell, Ctx, Taint, Value, ValueType, Word};
use crate::trace::fine;

impl Evaluator {
    /// Coerce a value to a string, collecting context into `ctx`.
    ///
    /// `coerce_more` also converts ints, floats, bools, null and lists (as
    /// `toString` does). `copy_to_store` turns paths into store paths.
    /// `canonicalize` is false only for the first segment of an
    /// interpolated path, preserving a trailing slash.
    #[allow(clippy::too_many_arguments)]
    pub fn coerce_to_string(
        &mut self,
        w: Word,
        ctx: &mut Vec<Ctx>,
        pos: Pos,
        err_ctx: &str,
        coerce_more: bool,
        copy_to_store: bool,
        canonicalize: bool,
    ) -> EvalResult<Cow<'static, [u8]>> {
        self.enter_call(pos)?;
        let r = self.coerce_inner(
            w,
            ctx,
            pos,
            err_ctx,
            coerce_more,
            copy_to_store,
            canonicalize,
        );
        self.call_depth -= 1;
        r
    }

    #[allow(clippy::too_many_arguments)]
    fn coerce_inner(
        &mut self,
        w: Word,
        ctx: &mut Vec<Ctx>,
        pos: Pos,
        err_ctx: &str,
        coerce_more: bool,
        copy_to_store: bool,
        canonicalize: bool,
    ) -> EvalResult<Cow<'static, [u8]>> {
        let w = self.force(w)?;
        match w.view() {
            Value::String(s) => {
                ctx.extend_from_slice(s.ctx);
                Ok(Cow::Borrowed(s.bytes))
            }
            Value::Path(p) => {
                if copy_to_store {
                    let sp = self.copy_path_to_store(p.path, pos)?;
                    coerce::path_copy(self, p.path, sp, pos)?;
                    ctx.push(Ctx::Opaque(sp));
                    Ok(Cow::Borrowed(sp))
                } else {
                    // the text of a path outside the store: marked so a
                    // derivation that receives it can be linted
                    if !self.is_store_path_text(p.path) {
                        ctx.push(Ctx::Impure(Taint::Path(p.path)));
                    }
                    Ok(Cow::Borrowed(p.path))
                }
            }
            Value::Attrs(a) => {
                if let Some(f) = a.get(self.s.to_string) {
                    let f = self.force_cell(&f.cell)?;
                    let r = self.call1(f, w, pos)?;
                    return self.coerce_to_string(
                        r,
                        ctx,
                        pos,
                        "while evaluating the result of the `__toString` attribute",
                        coerce_more,
                        copy_to_store,
                        canonicalize,
                    );
                }
                match a.get(self.s.out_path) {
                    Some(o) => {
                        let v = o.cell.get();
                        self.coerce_to_string(
                            v,
                            ctx,
                            pos,
                            err_ctx,
                            coerce_more,
                            copy_to_store,
                            canonicalize,
                        )
                    }
                    None => self.cannot_coerce(w, pos, err_ctx),
                }
            }
            Value::Bool(b) if coerce_more => Ok(Cow::Borrowed(if b { b"1" } else { b"" })),
            Value::Int(i) if coerce_more => Ok(Cow::Owned(i.to_string().into_bytes())),
            // std::to_string(double) is glibc's %f, which keeps a NaN's sign bit
            Value::Float(f) if coerce_more => Ok(Cow::Owned(
                match (f.is_nan(), f.is_sign_negative()) {
                    (true, true) => "-nan".to_string(),
                    (true, false) => "nan".to_string(),
                    _ => format!("{f:.6}"),
                }
                .into_bytes(),
            )),
            Value::Null if coerce_more => Ok(Cow::Borrowed(b"")),
            Value::List(l) if coerce_more => {
                let mut out: Vec<u8> = Vec::new();
                let cells = l.cells();
                for (n, c) in cells.iter().enumerate() {
                    let v = c.get();
                    let s = self
                        .coerce_to_string(
                            v,
                            ctx,
                            pos,
                            "while evaluating one element of the list",
                            coerce_more,
                            copy_to_store,
                            canonicalize,
                        )
                        .map_err(|mut e| {
                            e.add_trace(pos, err_ctx);
                            e
                        })?;
                    out.extend_from_slice(&s);
                    if n + 1 < cells.len() {
                        let v2 = self.force(v)?;
                        let empty_list = matches!(v2.view(), Value::List(l2) if l2.len == 0);
                        if !empty_list {
                            out.push(b' ');
                        }
                    }
                }
                Ok(Cow::Owned(out))
            }
            _ => self.cannot_coerce(w, pos, err_ctx),
        }
    }

    /// `path` lies under the store directory.
    pub fn is_store_path_text(&self, path: &[u8]) -> bool {
        path.starts_with(&self.store_dir)
            && path.get(self.store_dir.len()).is_none_or(|b| *b == b'/')
    }

    fn cannot_coerce<T>(&self, w: Word, pos: Pos, err_ctx: &str) -> EvalResult<T> {
        let mut e = Error::new(
            ErrorKind::Type,
            format!(
                "cannot coerce {} to a string: {}",
                w.value_type().name(),
                crate::print::show_value_short(self, w)
            ),
            pos,
        );
        if !err_ctx.is_empty() {
            e.add_trace(pos, err_ctx);
        }
        Err(Box::new(e))
    }

    pub fn copy_path_to_store(
        &mut self,
        path: &'static [u8],
        pos: Pos,
    ) -> EvalResult<&'static [u8]> {
        if path.ends_with(b".drv") {
            bail!(
                ErrorKind::Eval,
                pos,
                "file names are not allowed to end in '.drv'"
            );
        }
        coerce::store_path_recopy(self, path, pos)?;
        if !fine::on() {
            return store_hooks::copy_path_to_store(self, path, pos);
        }
        // a copy is cached, so a later reader takes the trace of the first
        if self.src_to_store.get(path).is_some() {
            fine::edge(fine::copy_trace(path));
            return store_hooks::copy_path_to_store(self, path, pos);
        }
        fine::push();
        let r = store_hooks::copy_path_to_store(self, path, pos);
        match r {
            Ok(_) => {
                let t = fine::pop();
                fine::set_copy_trace(path, t);
            }
            Err(_) => fine::pop_failed(),
        }
        r
    }

    /// The `==` operator. Nix compares two fresh operand temporaries
    /// (`ExprOpEq::eval`), so a function is never equal here.
    pub fn eq_values_top(&mut self, a: Word, b: Word, pos: Pos) -> EvalResult<bool> {
        let a = self.force(a)?;
        let b = self.force(b)?;
        if self.lint_on(LintId::EqTypeMismatch) || self.lint_on(LintId::EqFunction) {
            coerce::eq(self, a, b, pos)?;
        }
        // the operands are fresh copies in Nix, so its slot shortcut never
        // applies here: a function is unequal and a NaN is unequal to itself
        if a == b
            && (self.nix_eq || matches!(a.value_type(), ValueType::Function | ValueType::Float))
        {
            return self.eq_forced(a, b, pos);
        }
        self.eq_values(a, a, b, b, pos)
    }

    /// Nix `eqValues` on `a` and `b`, the words before forcing. Equal forced
    /// words are equal (the pointer shortcut, kept for speed where Nix would
    /// compare two slots); a function only when it is one slot, the word
    /// before forcing, since two thunks can force to one function.
    pub fn eq_values(
        &mut self,
        a: Word,
        fa: Word,
        b: Word,
        fb: Word,
        pos: Pos,
    ) -> EvalResult<bool> {
        if fa == fb && (a == b || (!self.nix_eq && fa.value_type() != ValueType::Function)) {
            return Ok(true);
        }
        self.eq_forced(fa, fb, pos)
    }

    fn eq_cells(&mut self, x: &Cell, y: &Cell, pos: Pos) -> EvalResult<bool> {
        let (a, b) = (x.get(), y.get());
        let fa = self.force_cell(x)?;
        let fb = self.force_cell(y)?;
        self.eq_values(a, fa, b, fb, pos)
    }

    fn eq_forced(&mut self, a: Word, b: Word, pos: Pos) -> EvalResult<bool> {
        match (a.view(), b.view()) {
            (Value::Int(x), Value::Int(y)) => Ok(x == y),
            (Value::Int(x), Value::Float(y)) => Ok(x as f64 == y),
            (Value::Float(x), Value::Int(y)) => Ok(x == y as f64),
            (Value::Float(x), Value::Float(y)) => Ok(x == y),
            (Value::Bool(x), Value::Bool(y)) => Ok(x == y),
            (Value::Null, Value::Null) => Ok(true),
            (Value::String(x), Value::String(y)) => Ok(a == b || x.bytes == y.bytes),
            (Value::Path(x), Value::Path(y)) => Ok(x.path == y.path),
            (Value::List(x), Value::List(y)) => {
                if x.len != y.len {
                    return Ok(false);
                }
                for (cx, cy) in x.cells().iter().zip(y.cells()) {
                    if !self.eq_cells(cx, cy, pos)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (Value::Attrs(x), Value::Attrs(y)) => {
                if self.is_derivation(a)? && self.is_derivation(b)? {
                    let i = x.get(self.s.out_path);
                    let j = y.get(self.s.out_path);
                    if let (Some(i), Some(j)) = (i, j) {
                        return self.eq_cells(&i.cell, &j.cell, pos);
                    }
                }
                if x.len() != y.len() {
                    return Ok(false);
                }
                for (ax, ay) in x.iter().zip(y.iter()) {
                    if ax.sym != ay.sym || !self.eq_cells(&ax.cell, &ay.cell, pos)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Nix `<` (also used by `lessThan` and `sort`).
    pub fn less_than(&mut self, a: Word, b: Word, pos: Pos) -> EvalResult<bool> {
        let a = self.force(a)?;
        let b = self.force(b)?;
        match (a.view(), b.view()) {
            (Value::Int(x), Value::Int(y)) => Ok(x < y),
            (Value::Float(x), Value::Int(y)) => Ok(x < y as f64),
            (Value::Int(x), Value::Float(y)) => Ok((x as f64) < y),
            (Value::Float(x), Value::Float(y)) => Ok(x < y),
            (Value::String(x), Value::String(y)) => Ok(x.bytes < y.bytes),
            (Value::Path(x), Value::Path(y)) => Ok(x.path < y.path),
            (Value::List(x), Value::List(y)) => {
                let (cx, cy) = (x.cells(), y.cells());
                let mut i = 0;
                loop {
                    if i == cy.len() {
                        return Ok(false);
                    }
                    if i == cx.len() {
                        return Ok(true);
                    }
                    if !self.eq_cells(&cx[i], &cy[i], pos)? {
                        return self.less_than(cx[i].get(), cy[i].get(), pos);
                    }
                    i += 1;
                }
            }
            _ => bail!(
                ErrorKind::Eval,
                pos,
                "cannot compare {} with {}; values are {} and {}",
                a.value_type().name(),
                b.value_type().name(),
                crate::print::show_value_short(self, a),
                crate::print::show_value_short(self, b)
            ),
        }
    }
}
