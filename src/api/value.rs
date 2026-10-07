//! The safe value handle an embedder works with.
//!
//! [`Eval`] borrows an [`Evaluator`]; [`Value`] is a value that evaluator
//! holds. The shared lifetime is the whole safety story: a `Value<'e>` can
//! only exist while the evaluator is borrowed, so it can never outlive the
//! arena it points into. Nothing here exposes `runtime::Word`, an arena
//! pointer or a tag.
//!
//! The layer is thin: every getter is the load the evaluator itself does.
//! Only the constructors allocate (docs/reference/library.md).

use std::fmt::{self, Debug, Formatter};
use std::marker::PhantomData;

use crate::eval::{Error, EvalResult, Evaluator, Pos};
use crate::runtime::value::Value as View;
use crate::runtime::{Ctx, Word};

use super::EvalArgs;
use crate::api;

const WHERE: &str = "while reading a value through the library API";

/// The type of a value, as Nix sees it. `Thunk` is the one mutable state:
/// it becomes one of the others once forced.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValueKind {
    Thunk,
    Int,
    Float,
    Bool,
    String,
    Path,
    Null,
    Attrs,
    List,
    Function,
}

impl ValueKind {
    /// The name `builtins.typeOf` gives; `"thunk"` has no Nix name.
    pub fn type_of(self) -> &'static str {
        match self {
            ValueKind::Thunk => "thunk",
            ValueKind::Int => "int",
            ValueKind::Float => "float",
            ValueKind::Bool => "bool",
            ValueKind::String => "string",
            ValueKind::Path => "path",
            ValueKind::Null => "null",
            ValueKind::Attrs => "set",
            ValueKind::List => "list",
            ValueKind::Function => "lambda",
        }
    }
}

/// One element of a string's context.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContextElem<'e> {
    /// A store path referenced as-is.
    Opaque(&'e [u8]),
    /// `=drv`: the whole derivation closure.
    DrvDeep(&'e [u8]),
    /// `!out!drv`: one output of a derivation.
    Built { drv: &'e [u8], output: &'e [u8] },
    /// An absolute path outside the store that a coercion kept as text, or
    /// the name of source tree metadata (`self.rev`). Invisible to
    /// `builtins.getContext`.
    Impure(&'e [u8]),
}

/// A value of the evaluator `Eval` borrows. Copy, one machine word, and
/// invariant in `'e` so it stays with the evaluator that made it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Value<'e> {
    w: Word,
    ev: PhantomData<fn(&'e ()) -> &'e ()>,
}
#[inline]
fn val<'e>(w: Word) -> Value<'e> {
    Value { w, ev: PhantomData }
}

impl Debug for Value<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Value({})", self.kind().type_of())
    }
}

impl<'e> Value<'e> {
    /// The forced view, or `None` while the value is still a thunk.
    #[inline]
    fn view(self) -> Option<View> {
        if self.w == Word::EMPTY || self.w.is_thunk() || self.w.is_blackhole() {
            None
        } else {
            Some(self.w.view())
        }
    }

    #[inline]
    pub fn kind(self) -> ValueKind {
        let Some(v) = self.view() else {
            return ValueKind::Thunk;
        };
        match v {
            View::Int(_) => ValueKind::Int,
            View::Float(_) => ValueKind::Float,
            View::Bool(_) => ValueKind::Bool,
            View::Null => ValueKind::Null,
            View::String(_) => ValueKind::String,
            View::Path(_) => ValueKind::Path,
            View::List(_) => ValueKind::List,
            View::Attrs(_) => ValueKind::Attrs,
            View::Lambda(_) | View::PrimOp(_) | View::PrimOpApp(_) => ValueKind::Function,
        }
    }

    #[inline]
    pub fn as_int(self) -> Option<i64> {
        match self.view()? {
            View::Int(i) => Some(i),
            _ => None,
        }
    }

    /// The value as a float; an integer widens, as `force_float` does.
    #[inline]
    pub fn as_float(self) -> Option<f64> {
        match self.view()? {
            View::Float(f) => Some(f),
            View::Int(i) => Some(i as f64),
            _ => None,
        }
    }

    #[inline]
    pub fn as_bool(self) -> Option<bool> {
        match self.view()? {
            View::Bool(b) => Some(b),
            _ => None,
        }
    }

    #[inline]
    pub fn is_null(self) -> bool {
        matches!(self.view(), Some(View::Null))
    }

    /// The bytes of a string, borrowed from the arena. Nix strings are
    /// arbitrary bytes, so this is not `&str`.
    #[inline]
    pub fn as_bytes(self) -> Option<&'e [u8]> {
        match self.view()? {
            View::String(s) => Some(s.bytes),
            _ => None,
        }
    }

    /// The string's context, empty for every other type.
    pub fn context(self) -> impl ExactSizeIterator<Item = ContextElem<'e>> {
        let ctx: &'static [Ctx] = match self.view() {
            Some(View::String(s)) => s.ctx,
            _ => &[],
        };
        ctx.iter().map(|c| match *c {
            Ctx::Opaque(p) => ContextElem::Opaque(p),
            Ctx::DrvDeep(p) => ContextElem::DrvDeep(p),
            Ctx::Built { drv, output } => ContextElem::Built { drv, output },
            Ctx::Impure(m) => ContextElem::Impure(m.bytes()),
        })
    }

    #[inline]
    pub fn as_path(self) -> Option<&'e [u8]> {
        match self.view()? {
            View::Path(p) => Some(p.path),
            _ => None,
        }
    }

    #[inline]
    pub fn list_len(self) -> Option<usize> {
        match self.view()? {
            View::List(l) => Some(l.len as usize),
            _ => None,
        }
    }

    /// Element `i`, unforced.
    #[inline]
    pub fn list_get(self, i: usize) -> Option<Value<'e>> {
        match self.view()? {
            View::List(l) => l.cells().get(i).map(|c| val(c.get())),
            _ => None,
        }
    }

    #[inline]
    pub fn attrs_len(self) -> Option<usize> {
        match self.view()? {
            View::Attrs(a) => Some(a.len()),
            _ => None,
        }
    }
}

/// An evaluator, borrowed for the life of the values it hands out. Every
/// operation that may evaluate, intern a name or allocate goes through it.
pub struct Eval<'e> {
    ev: &'e mut Evaluator,
}

impl<'e> Eval<'e> {
    pub fn new(ev: &'e mut Evaluator) -> Eval<'e> {
        Eval { ev }
    }

    /// The evaluator itself, for the parts of the library that take one.
    pub fn evaluator(&mut self) -> &mut Evaluator {
        self.ev
    }

    /// Evaluate an expression; relative paths resolve against `base_dir`.
    pub fn eval_expr(&mut self, src: &str, base_dir: &[u8]) -> EvalResult<Value<'e>> {
        self.ev.eval_string(src, base_dir).map(val)
    }

    /// Evaluate what `args` names: an expression, a file or a flake output.
    pub fn evaluate(&mut self, args: EvalArgs) -> Result<Value<'e>, Box<Error>> {
        api::evaluate(self.ev, args).map(val)
    }

    /// To weak head normal form: the value's own type becomes known.
    #[inline]
    pub fn force(&mut self, v: Value<'e>) -> EvalResult<Value<'e>> {
        self.ev.force(v.w).map(val)
    }

    /// Recursively, through lists and attribute sets.
    pub fn force_deep(&mut self, v: Value<'e>) -> EvalResult<Value<'e>> {
        self.ev.force_deep(v.w).map(val)
    }

    /// The attribute `name`, unforced, or `None` when the set has none.
    /// Errors when `v` is not an attribute set.
    pub fn attr(&mut self, v: Value<'e>, name: &[u8]) -> EvalResult<Option<Value<'e>>> {
        let a = self.ev.force_attrs(v.w, Pos::NONE, WHERE)?;
        // a name no source ever mentioned cannot be an attribute
        let Some(sym) = self.ev.interner.get(name) else {
            return Ok(None);
        };
        Ok(a.get(sym).map(|at| val(at.cell.get())))
    }

    pub fn has_attr(&mut self, v: Value<'e>, name: &[u8]) -> EvalResult<bool> {
        Ok(self.attr(v, name)?.is_some())
    }

    /// Attribute `i` in the set's own order: its name and its unforced
    /// value. The order is not alphabetical (docs/reference/library.md).
    pub fn attr_at(&mut self, v: Value<'e>, i: usize) -> EvalResult<Option<(&'e [u8], Value<'e>)>> {
        let a = self.ev.force_attrs(v.w, Pos::NONE, WHERE)?;
        // `//` keeps both sides; index order needs the merged copy
        let a = if a.is_union() {
            self.ev.arena.flat(a)
        } else {
            a
        };
        let Some(at) = a.attrs().get(i) else {
            return Ok(None);
        };
        Ok(Some((self.ev.interner.name(at.sym), val(at.cell.get()))))
    }

    /// Apply a function value to one argument.
    pub fn call(&mut self, f: Value<'e>, arg: Value<'e>) -> EvalResult<Value<'e>> {
        self.ev.call1(f.w, arg.w, Pos::NONE).map(val)
    }

    /// The same application as a thunk: nothing runs until it is forced.
    pub fn apply(&mut self, f: Value<'e>, arg: Value<'e>) -> Value<'e> {
        val(self.ev.arena.mk_thunk_app(f.w, arg.w))
    }

    pub fn int(&mut self, i: i64) -> Value<'e> {
        val(self.ev.mk_int(i))
    }

    pub fn float(&mut self, f: f64) -> Value<'e> {
        val(self.ev.mk_float(f))
    }

    pub fn boolean(&mut self, b: bool) -> Value<'e> {
        val(Word::from_bool(b))
    }

    pub fn null(&mut self) -> Value<'e> {
        val(Word::NULL)
    }

    /// A string without context; the bytes are copied into the arena.
    pub fn string(&mut self, bytes: &[u8]) -> Value<'e> {
        val(self.ev.mk_string(bytes))
    }

    /// A path; the bytes are copied into the arena.
    pub fn path(&mut self, path: &[u8]) -> Value<'e> {
        val(self.ev.arena.mk_path(path))
    }

    /// A list. The elements are gathered into a temporary before the copy.
    pub fn list(&mut self, items: &[Value<'e>]) -> Value<'e> {
        let words: Vec<Word> = items.iter().map(|v| v.w).collect();
        val(self.ev.mk_list(&words))
    }

    /// An attribute set. Errors on a repeated name.
    pub fn attrs(&mut self, entries: &[(&[u8], Value<'e>)]) -> EvalResult<Value<'e>> {
        let mut es: Vec<_> = entries
            .iter()
            .map(|(n, v)| (self.ev.sym(n), Pos::NONE, v.w))
            .collect();
        self.ev.mk_attrs(&mut es, Pos::NONE).map(val)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval<R>(src: &str, f: impl for<'e> FnOnce(&mut Eval<'e>, Value<'e>) -> R) -> R {
        let mut ev = Evaluator::new();
        let mut e = Eval::new(&mut ev);
        let v = e.eval_expr(src, b"/").expect("eval");
        f(&mut e, v)
    }

    #[test]
    fn scalars() {
        eval("1", |e, v| {
            let v = e.force(v).unwrap();
            assert_eq!(v.kind(), ValueKind::Int);
            assert_eq!(v.as_int(), Some(1));
            assert_eq!(v.as_float(), Some(1.0));
            assert_eq!(v.as_bool(), None);
        });
        eval("1.5", |e, v| {
            assert_eq!(e.force(v).unwrap().as_float(), Some(1.5))
        });
        eval("true", |e, v| {
            assert_eq!(e.force(v).unwrap().as_bool(), Some(true))
        });
        eval("null", |e, v| assert!(e.force(v).unwrap().is_null()));
        eval("/a/b", |e, v| {
            let v = e.force(v).unwrap();
            assert_eq!(v.kind(), ValueKind::Path);
            assert_eq!(v.as_path(), Some(&b"/a/b"[..]));
        });
        eval("x: x", |e, v| {
            assert_eq!(e.force(v).unwrap().kind(), ValueKind::Function)
        });
        eval("builtins.add", |e, v| {
            assert_eq!(e.force(v).unwrap().kind(), ValueKind::Function)
        });
    }

    /// A getter on an unforced value reads nothing; it reports `Thunk`.
    #[test]
    fn thunk_reads_as_thunk() {
        eval("{ a = (z: z + 1) 1; }", |e, v| {
            let a = e.attr(v, b"a").unwrap().unwrap();
            assert_eq!(a.kind(), ValueKind::Thunk);
            assert_eq!(a.as_int(), None);
            assert_eq!(a.attrs_len(), None);
            assert!(!a.is_null());
            assert_eq!(e.force(a).unwrap().as_int(), Some(2));
        });
    }

    #[test]
    fn strings_and_context() {
        eval(r#""hi""#, |e, v| {
            let v = e.force(v).unwrap();
            assert_eq!(v.as_bytes(), Some(&b"hi"[..]));
            assert_eq!(v.context().len(), 0);
        });
        let p = "/nix/store/00000000000000000000000000000000-foo";
        let src = format!(r#"builtins.appendContext "x" {{ "{p}" = {{ path = true; }}; }}"#);
        eval(&src, |e, v| {
            let v = e.force(v).unwrap();
            assert_eq!(v.as_bytes(), Some(&b"x"[..]));
            let ctx: Vec<_> = v.context().collect();
            assert_eq!(ctx, vec![ContextElem::Opaque(p.as_bytes())]);
        });
        eval("toString ./a", |e, v| {
            let v = e.force(v).unwrap();
            assert!(matches!(
                v.context().next(),
                Some(ContextElem::Impure(b"/a"))
            ));
        });
    }

    #[test]
    fn lists() {
        eval("[ 1 2 3 ]", |e, v| {
            let v = e.force(v).unwrap();
            assert_eq!(v.list_len(), Some(3));
            assert_eq!(v.attrs_len(), None);
            let second = v.list_get(1).unwrap();
            assert_eq!(e.force(second).unwrap().as_int(), Some(2));
            assert!(v.list_get(3).is_none());
        });
    }

    #[test]
    fn attrs_by_name_and_index() {
        eval("{ b = 2; a = 1; }", |e, v| {
            let v = e.force(v).unwrap();
            assert_eq!(v.attrs_len(), Some(2));
            let a = e.attr(v, b"a").unwrap().unwrap();
            assert_eq!(e.force(a).unwrap().as_int(), Some(1));
            assert!(e.has_attr(v, b"b").unwrap());
            // index order is the evaluator's, as in nix_get_attr_byidx
            let mut seen = Vec::new();
            for i in 0..v.attrs_len().unwrap() {
                let (name, av) = e.attr_at(v, i).unwrap().unwrap();
                seen.push((name.to_vec(), e.force(av).unwrap().as_int().unwrap()));
            }
            seen.sort();
            assert_eq!(seen, vec![(b"a".to_vec(), 1), (b"b".to_vec(), 2)]);
            assert!(e.attr_at(v, 2).unwrap().is_none());
        });
    }

    /// A name the evaluation never saw is a miss, not an error.
    #[test]
    fn missing_attribute() {
        eval("{ a = 1; }", |e, v| {
            let v = e.force(v).unwrap();
            assert!(e.attr(v, b"nope").unwrap().is_none());
            assert!(!e.has_attr(v, b"nope").unwrap());
        });
        eval("{ a = 1; } // { b = 2; }", |e, v| {
            let v = e.force(v).unwrap();
            assert_eq!(v.attrs_len(), Some(2));
            assert!(e.attr(v, b"c").unwrap().is_none());
            let mut names: Vec<_> = (0..2)
                .map(|i| e.attr_at(v, i).unwrap().unwrap().0)
                .collect();
            names.sort();
            assert_eq!(names, vec![&b"a"[..], &b"b"[..]]);
        });
    }

    #[test]
    fn wrong_type_is_an_error() {
        eval("1", |e, v| {
            let v = e.force(v).unwrap();
            let err = e.attr(v, b"a").unwrap_err();
            assert!(err.msg.contains("expected a set"), "{}", err.msg);
        });
    }

    #[test]
    fn calling_a_function() {
        eval("x: x + 1", |e, v| {
            let f = e.force(v).unwrap();
            let arg = e.int(41);
            let r = e.call(f, arg).unwrap();
            assert_eq!(e.force(r).unwrap().as_int(), Some(42));
        });
        eval("s: s.name", |e, v| {
            let f = e.force(v).unwrap();
            let name = e.string(b"hi");
            let arg = e.attrs(&[(&b"name"[..], name)]).unwrap();
            let r = e.call(f, arg).unwrap();
            assert_eq!(e.force(r).unwrap().as_bytes(), Some(&b"hi"[..]));
        });
    }

    #[test]
    fn construction() {
        eval("builtins.typeOf", |e, v| {
            let f = e.force(v).unwrap();
            let cases: Vec<(Value<'_>, &str)> = vec![
                (e.int(1), "int"),
                (e.float(1.0), "float"),
                (e.boolean(true), "bool"),
                (e.null(), "null"),
                (e.string(b"s"), "string"),
                (e.path(b"/a"), "path"),
            ];
            for (v, want) in cases {
                assert_eq!(v.kind().type_of(), want);
                let r = e.call(f, v).unwrap();
                assert_eq!(e.force(r).unwrap().as_bytes(), Some(want.as_bytes()));
            }
            let one = e.int(1);
            let l = e.list(&[one, one]);
            assert_eq!(l.list_len(), Some(2));
            let a = e.attrs(&[(&b"x"[..], one)]).unwrap();
            assert_eq!(a.attrs_len(), Some(1));
            assert!(e.attrs(&[(&b"x"[..], one), (&b"x"[..], one)]).is_err());
        });
    }

    /// `apply` defers the call; `force` is what runs it.
    #[test]
    fn apply_is_lazy() {
        eval("x: x + 1", |e, v| {
            let f = e.force(v).unwrap();
            let arg = e.int(41);
            let t = e.apply(f, arg);
            assert_eq!(t.kind(), ValueKind::Thunk);
            assert_eq!(e.force(t).unwrap().as_int(), Some(42));
        });
    }

    #[test]
    fn force_deep_forces_the_whole_tree() {
        eval("{ a = [ (1 + 1) ]; }", |e, v| {
            let v = e.force_deep(v).unwrap();
            let a = e.attr(v, b"a").unwrap().unwrap();
            assert_eq!(a.list_get(0).unwrap().as_int(), Some(2));
        });
    }
}
