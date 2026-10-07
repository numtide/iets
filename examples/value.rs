//! Walk a value with the safe value API.
//!
//! `cargo run --example value -- [-E EXPR]`
//!
//! Uses the library only (`iets::api::value`), never `iets::cli`.

use iets::api::value::{ContextElem, Eval, Value, ValueKind};
use iets::eval::Evaluator;

use std::env;
use std::process::{self, ExitCode};

const DEFAULT: &str = r#"
  rec {
    name = "demo";
    version = 3;
    ratio = 0.5;
    enabled = true;
    nothing = null;
    dir = /tmp;
    tags = [ "one" "two" ];
    greet = who: "hello ${who}";
    nested = { inherit name; };
  }
"#;

fn main() -> ExitCode {
    // a frontend owns the run state and says where messages go
    let session = iets::session::Session::new();
    session.log.write_to_stderr();
    let _entered = session.enter();
    let mut argv = env::args().skip(1);
    let src = match argv.next().as_deref() {
        None => DEFAULT.to_string(),
        Some("-E") | Some("--expr") => argv.next().unwrap_or_else(|| die("-E needs an EXPR")),
        Some(f) => die(&format!("unknown flag {f}")),
    };

    let mut evaluator = Evaluator::new();
    let mut e = Eval::new(&mut evaluator);
    let root = match e.eval_expr(&src, b"/").and_then(|v| e.force(v)) {
        Ok(v) => v,
        Err(err) => {
            iets::api::report(e.evaluator(), &err);
            return ExitCode::from(1);
        }
    };

    println!("root is {}", root.kind().type_of());
    let Some(n) = root.attrs_len() else {
        return ExitCode::SUCCESS;
    };

    // index order is the evaluator's own, so sort for a stable listing
    let mut names: Vec<Vec<u8>> = Vec::with_capacity(n);
    for i in 0..n {
        let (name, _) = e.attr_at(root, i).expect("a set").expect("in range");
        names.push(name.to_vec());
    }
    names.sort();

    for name in &names {
        let v = e.attr(root, name).expect("a set").expect("just listed");
        let v = match e.force(v) {
            Ok(v) => v,
            Err(err) => {
                iets::api::report(e.evaluator(), &err);
                return ExitCode::from(1);
            }
        };
        println!(
            "  {} : {} = {}",
            show(name),
            v.kind().type_of(),
            show_value(v)
        );
    }

    // read two fields by name, then call the function value with an argument
    if let Ok(Some(v)) = e.attr(root, b"version")
        && let Ok(v) = e.force(v)
    {
        println!("version + 0 = {:?}", v.as_int());
    }
    if let Ok(Some(f)) = e.attr(root, b"greet")
        && let Ok(f) = e.force(f)
    {
        let arg = e.string(b"world");
        match e.call(f, arg).and_then(|r| e.force(r)) {
            Ok(r) => println!("greet \"world\" = {}", show_value(r)),
            Err(err) => {
                iets::api::report(e.evaluator(), &err);
            }
        }
    }
    ExitCode::SUCCESS
}

fn show_value(v: Value<'_>) -> String {
    match v.kind() {
        ValueKind::Int => format!("{}", v.as_int().unwrap()),
        ValueKind::Float => format!("{}", v.as_float().unwrap()),
        ValueKind::Bool => format!("{}", v.as_bool().unwrap()),
        ValueKind::Null => "null".to_string(),
        ValueKind::Path => show(v.as_path().unwrap()),
        ValueKind::String => {
            let mut s = format!("\"{}\"", show(v.as_bytes().unwrap()));
            for c in v.context() {
                let p = match c {
                    ContextElem::Opaque(p)
                    | ContextElem::DrvDeep(p)
                    | ContextElem::Impure(p)
                    | ContextElem::Built { drv: p, .. } => p,
                };
                s.push_str(&format!(" [{}]", show(p)));
            }
            s
        }
        ValueKind::List => format!("{} elements", v.list_len().unwrap()),
        ValueKind::Attrs => format!("{} attributes", v.attrs_len().unwrap()),
        ValueKind::Function | ValueKind::Thunk => v.kind().type_of().to_string(),
    }
}

fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn die(msg: &str) -> ! {
    eprintln!("usage: value [-E EXPR]: {msg}");
    process::exit(2)
}
