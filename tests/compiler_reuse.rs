//! Reusing the public compiler must not inherit an earlier attempt's scope.
use iets::compile::ir::{Ir, IrRef};
use iets::compile::{Compiler, Globals};
use iets::lint::{Config, Level, LintId};
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
    let mut lints = Config::default();
    lints.levels.fill(Level::Off);
    lints.levels[LintId::UrlLiteral as usize] = Level::Warn;
    let mut compiler = Compiler::new(
        &ast,
        &mut arena,
        &mut interner,
        &globals,
        1,
        b"/",
        Some(&lints),
    );
    run(&mut compiler, x, y)
}

fn shape(ir: IrRef) -> String {
    match ir {
        Ir::Local { depth, idx } => format!("{depth}:{idx}"),
        Ir::List(items) => items
            .iter()
            .copied()
            .map(shape)
            .collect::<Vec<_>>()
            .join(","),
        Ir::Lambda(l) => format!(
            "({},{},{},{}:{})",
            l.closed,
            l.no_env,
            l.base,
            l.frame,
            shape(l.body)
        ),
        _ => panic!("unexpected IR in the capture fixture"),
    }
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

#[test]
fn a_successful_retry_has_fresh_lambda_captures() {
    let source = b"a: b: [ a b x ]";
    let reused = compile(source, |c, x, _| {
        assert!(c.compile_root(None).is_err());
        let first = shape(c.compile_root(Some(&[x])).unwrap());
        assert_eq!(shape(c.compile_root(Some(&[x])).unwrap()), first);
        first
    });
    let fresh = compile(source, |c, x, _| shape(c.compile_root(Some(&[x])).unwrap()));
    assert_eq!(reused, fresh);
}

#[test]
fn diagnostics_and_home_remain_under_the_callers_control() {
    let source = b"[ https://example.org x ~/compiler-reuse y ]";
    compile(source, |c, x, y| {
        assert!(c.compile_root(Some(&[x])).is_err());
        assert!(c.used_home.get());
        assert_eq!(c.diags.len(), 1);
        let warning = c.diags.clone();
        c.compile_root(Some(&[x, y])).unwrap();
        let successful = (c.used_home.get(), c.diags.clone());
        // Success already released the old scopes before this fix: this
        // early error isolates the public observers from binding leaks.
        let error = c.compile_root(None).err().expect("x must be undefined");
        assert!(error.msg.contains("x"));
        let failed = (c.used_home.get(), c.diags.clone());
        c.compile_root(Some(&[x, y])).unwrap();
        let repeated = (c.used_home.get(), c.diags.clone());
        let expected = |n| (true, vec![warning[0].clone(); n]);
        assert_eq!(
            [successful, failed, repeated],
            [expected(2), expected(3), expected(4)]
        );
        c.diags.clear();
        c.used_home.set(false);
        assert!(c.compile_root(None).is_err());
        assert!(!c.used_home.get());
        assert_eq!(c.diags, warning);
    });
}
