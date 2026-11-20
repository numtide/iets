//! Reusing the public compiler must not inherit an earlier attempt's scope.
use iets::compile::{Compiler, Globals};
use iets::runtime::Arena;
use iets::session::Session;
use iets::syntax;
use iets::syntax::ast::FileId;
use iets::syntax::symbol::{Interner, Sym};

fn compile<T>(source: &[u8], run: impl FnOnce(&mut Compiler<'_>, Sym, Sym) -> T) -> T {
    let session = Session::new();
    let _entered = session.enter();
    let mut interner = Interner::new();
    let ast = syntax::parse(source, FileId(0), &mut interner).unwrap();
    let x = interner.intern(b"x");
    let y = interner.intern(b"y");
    let mut arena = Arena::default();
    let globals = Globals {
        table: Default::default(),
        builtins: None,
    };
    let mut compiler = Compiler::new(&ast, &mut arena, &mut interner, &globals, 1, b"/", None);
    run(&mut compiler, x, y)
}

#[test]
fn failed_root_does_not_supply_the_next_roots_bindings() {
    for source in [
        &b"[ x y ]"[..],
        b"a: let b = a; in [ b x y ]",
        b"rec { a = x; b = y; }",
        b"{ a ? x, ... }: [ a y ]",
    ] {
        let reused = compile(source, |c, x, y| {
            assert!(c.compile_root(Some(&[x])).is_err());
            let error = c
                .compile_root(Some(&[y]))
                .err()
                .expect("x leaked from the failed root");
            c.compile_root(Some(&[x, y])).unwrap();
            (error.kind, error.msg, error.pos)
        });
        let fresh = compile(source, |c, _, y| {
            let error = c
                .compile_root(Some(&[y]))
                .err()
                .expect("fixture must require x");
            (error.kind, error.msg, error.pos)
        });
        assert_eq!(reused, fresh);
    }
}
