//! `builtins.exec` (primops.cc:prim_exec): run a program and evaluate its
//! stdout as a Nix expression. It exists only when
//! `allow-unsafe-native-code-during-evaluation` is on, and pure evaluation
//! refuses it (docs/reference/builtins.md).

use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

use super::Registry;
use super::paths::lossy;
use super::store_hooks;
use crate::eval::{Error, ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::runtime::{Ctx, Word};
use crate::{config, error, log, parallel, trace};

pub const SETTING: &str = "allow-unsafe-native-code-during-evaluation";

pub fn register(r: &mut Registry) {
    r.op("__exec", 1, exec);
}

/// The fix for a missing `builtins.exec` or `__exec`, when the setting is
/// what is missing.
pub fn hint(name: &str) -> Option<String> {
    (matches!(name, "exec" | "__exec") && !config::native_code())
        .then(|| format!("builtins.exec needs '--option {SETTING} true'"))
}

fn exec(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    if ev.pure() {
        bail!(
            ErrorKind::Eval,
            pos,
            "'builtins.exec' is not allowed in pure evaluation mode (use '--impure' to override)"
        );
    }
    let l = ev.force_list(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.exec",
    )?;
    let Some((first, rest)) = l.cells().split_first() else {
        bail!(
            ErrorKind::Eval,
            pos,
            "at least one argument to 'exec' required"
        );
    };
    let mut ctx = Vec::new();
    let program = ev
        .coerce_to_string(
            first.get(),
            &mut ctx,
            pos,
            "while evaluating the first element of the argument passed to builtins.exec",
            false,
            false,
            true,
        )?
        .into_owned();
    let mut argv = Vec::with_capacity(rest.len());
    for c in rest {
        let a = ev.coerce_to_string(
            c.get(),
            &mut ctx,
            pos,
            "while evaluating an element of the argument passed to builtins.exec",
            false,
            false,
            true,
        )?;
        argv.push(a.into_owned());
    }
    let shown = lossy(&program);
    realise(ev, &ctx, &shown, pos)?;
    // a helper must not start a program the program may never ask for
    if parallel::journal(ev, String::new()) {
        return Err(Error::boxed(ErrorKind::Cancelled, "cancelled", Pos::NONE));
    }
    // the read set cannot hold what a program would print next time
    ev.trace().mark_uncacheable();
    let out = Command::new(OsStr::from_bytes(&program))
        .args(argv.iter().map(|a| OsStr::from_bytes(a)))
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output();
    let failed =
        |why: String| Error::boxed(ErrorKind::Exec, format!("program \"{shown}\" {why}"), pos);
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            // processes.cc: the child reports a failed exec and exits 1
            log::line(&format!(
                "error: executing \"{shown}\": {}",
                error::io_text(&e)
            ));
            return Err(failed("failed with exit code 1".into()));
        }
    };
    if let Some(sig) = out.status.signal() {
        // SAFETY: strsignal returns a NUL-terminated string that stays valid
        // until the next call on this thread, and it is copied before that
        let name = unsafe { CStr::from_ptr(libc::strsignal(sig)) }.to_string_lossy();
        return Err(failed(format!("failed due to signal {sig} ({name})")));
    }
    if let Some(code) = out.status.code().filter(|c| *c != 0) {
        return Err(failed(format!("failed with exit code {code}")));
    }
    let ir = ev
        .compile_source(b"\xc2\xabstring\xc2\xbb", out.stdout, b"/", None)
        .map_err(|mut e| {
            e.add_trace(pos, format!("while parsing the output from '{shown}'"));
            e
        })?;
    let root = ev.root_env;
    ev.eval(root, ir).map_err(|mut e| {
        e.add_trace(pos, format!("while evaluating the output from '{shown}'"));
        e
    })
}

/// primops.cc:realiseContext without building. A store path iets only
/// holds in memory or as a local tree cannot be run or read by a child.
fn realise(ev: &mut Evaluator, ctx: &[Ctx], shown: &str, pos: Pos) -> EvalResult<()> {
    for c in ctx {
        let (kind, msg) = match c {
            Ctx::Impure(_) => continue,
            Ctx::Opaque(sp) | Ctx::DrvDeep(sp) => {
                if ev.trace().stat_now(sp, true) != trace::Kind::Missing {
                    continue;
                }
                match store_hooks::locate(ev, sp) {
                    Some(_) => (
                        ErrorKind::Unsupported,
                        format!(
                            "iets: builtins.exec needs '{}' in the store, and iets does not write the store",
                            lossy(sp)
                        ),
                    ),
                    None => (
                        ErrorKind::Eval,
                        format!(
                            "cannot execute '{shown}', since store path '{}' is not valid",
                            lossy(sp.rsplit(|b| *b == b'/').next().unwrap_or(sp))
                        ),
                    ),
                }
            }
            Ctx::Built { drv, .. } => (
                ErrorKind::Unsupported,
                format!(
                    "iets: builtins.exec cannot build '{}' during evaluation",
                    lossy(drv)
                ),
            ),
        };
        bail!(kind, pos, "{msg}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::env;

    use super::SETTING;
    use crate::config::Config;
    use crate::eval::{Error, ErrorKind, EvalConfig, Evaluator};
    use crate::print;
    use crate::session::Session;

    fn session(on: bool) -> std::sync::Arc<Session> {
        let s = Session::new();
        let mut c = Config::empty();
        c.set(SETTING, if on { "true" } else { "false" });
        let _ = s.nix_conf.set(c);
        s
    }

    fn run(on: bool, pure: bool, src: &str) -> (Evaluator, Result<String, Box<Error>>) {
        let s = session(on);
        let _g = s.enter();
        let mut ev = EvalConfig::new()
            .session(s.clone())
            .pure(pure)
            .trace(true)
            .build();
        let r = ev.eval_string(src, b"/").and_then(|v| ev.force_deep(v));
        let r = r.map(|v| {
            let mut out = Vec::new();
            print::print_ambiguous(&mut ev, v, 1, &mut out, &mut HashSet::new(), 0).unwrap();
            String::from_utf8(out).unwrap()
        });
        (ev, r)
    }

    fn ok(src: &str) -> String {
        run(true, false, src)
            .1
            .unwrap_or_else(|e| panic!("{src}: {}", e.msg))
    }

    fn err(on: bool, pure: bool, src: &str) -> Box<Error> {
        match run(on, pure, src).1 {
            Ok(v) => panic!("{src}: expected an error, got {v}"),
            Err(e) => e,
        }
    }

    fn traces(e: &Error) -> Vec<&str> {
        e.trace.iter().map(|(_, m)| m.as_str()).collect()
    }

    /// Nix 2.34.8 without the setting: no `exec` at all (primops.cc).
    #[test]
    fn off_by_default_and_the_error_names_the_setting() {
        assert_eq!(run(false, false, "builtins ? exec").1.unwrap(), "false");
        let fix = Some(format!("builtins.exec needs '--option {SETTING} true'"));
        for (src, kind, msg) in [
            (
                "builtins.exec [ \"true\" ]",
                ErrorKind::MissingAttr,
                "attribute 'exec' missing",
            ),
            (
                "__exec [ \"true\" ]",
                ErrorKind::UndefinedVar,
                "undefined variable '__exec'",
            ),
            (
                "with builtins; exec [ \"true\" ]",
                ErrorKind::UndefinedVar,
                "undefined variable 'exec'",
            ),
        ] {
            let e = err(false, false, src);
            assert_eq!(
                (e.kind, e.msg.as_str(), &e.hint),
                (kind, msg, &fix),
                "{src}"
            );
        }
        assert_eq!(err(false, false, "{ }.exec").hint, None);
        assert_eq!(run(true, false, "builtins ? exec").1.unwrap(), "true");
    }

    /// Nix runs `exec` in pure mode too; iets refuses it, and says how to
    /// get past that (docs/reference/divergences.md).
    #[test]
    fn pure_evaluation_refuses_it() {
        assert_eq!(run(true, true, "builtins ? exec").1.unwrap(), "true");
        let e = err(true, true, "builtins.exec [ \"true\" ]");
        assert_eq!(
            e.msg,
            "'builtins.exec' is not allowed in pure evaluation mode (use '--impure' to override)"
        );
    }

    /// The answers of `nix-instantiate --eval --strict --option
    /// allow-unsafe-native-code-during-evaluation true` (Nix 2.34.8).
    #[test]
    fn runs_a_program_and_evaluates_its_output() {
        assert_eq!(ok(r#"builtins.exec [ "echo" "{ a = 1; }" ]"#), "{ a = 1; }");
        assert_eq!(ok(r#"__exec [ "printf" "[ %s %s ]" "1" "2" ]"#), "[ 1 2 ]");
        assert_eq!(ok(r#"builtins.exec [ "echo" "1 + 2 # c" ]"#), "3");
        assert_eq!(ok(r#"builtins.exec [ /bin/sh "-c" "echo 1" ]"#), "1");
        assert_eq!(ok(r#"builtins.exec [ "echo" "./a" ]"#), "/a");
        assert_eq!(ok(r#"builtins.exec [ "echo" "x: x" ]"#), "<LAMBDA>");
        let home = env::var("HOME").unwrap_or_default();
        let cwd = env::current_dir().unwrap();
        assert_eq!(
            ok(r#"builtins.exec [ "sh" "-c" "echo \"\\\"$HOME\\\"\"" ]"#),
            format!("{home:?}")
        );
        assert_eq!(
            ok(r#"builtins.exec [ "sh" "-c" "echo \"\\\"$PWD\\\"\"" ]"#),
            format!("{:?}", cwd.display().to_string())
        );
        let e = err(true, false, r#"let x = 1; in builtins.exec [ "echo" "x" ]"#);
        assert_eq!(e.msg, "undefined variable 'x'");
    }

    #[test]
    fn argument_shapes() {
        let cases = [
            (
                r#"builtins.exec "x""#,
                "expected a list but found a string: \"x\"",
                Some("while evaluating the first argument passed to builtins.exec"),
            ),
            (
                "builtins.exec [ ]",
                "at least one argument to 'exec' required",
                None,
            ),
            (
                "builtins.exec [ 1 ]",
                "cannot coerce an integer to a string: 1",
                Some("while evaluating the first element of the argument passed to builtins.exec"),
            ),
            (
                r#"builtins.exec [ "echo" { b = 2; } ]"#,
                "cannot coerce a set to a string: { b = 2; }",
                Some("while evaluating an element of the argument passed to builtins.exec"),
            ),
        ];
        for (src, msg, trace) in cases {
            let e = err(true, false, src);
            assert_eq!(e.msg, msg, "{src}");
            if let Some(t) = trace {
                assert!(traces(&e).contains(&t), "{src}: {:?}", traces(&e));
            }
        }
    }

    #[test]
    fn a_failed_program_is_an_exec_error() {
        let cases = [
            (
                r#"[ "sh" "-c" "exit 3" ]"#,
                "program \"sh\" failed with exit code 3",
            ),
            (
                r#"[ "sh" "-c" "kill -9 $$" ]"#,
                "program \"sh\" failed due to signal 9 (Killed)",
            ),
            (
                r#"[ "iets-no-such-program" ]"#,
                "program \"iets-no-such-program\" failed with exit code 1",
            ),
        ];
        for (args, msg) in cases {
            let e = err(true, false, &format!("builtins.exec {args}"));
            assert_eq!((e.kind, e.msg.as_str()), (ErrorKind::Exec, msg), "{args}");
        }
        let e = err(
            true,
            false,
            r#"builtins.tryEval (builtins.exec [ "false" ])"#,
        );
        assert_eq!(e.kind, ErrorKind::Exec);
    }

    #[test]
    fn the_output_is_parsed_and_evaluated_as_a_string_source() {
        let (ev, r) = run(true, false, r#"builtins.exec [ "echo" "{ x =" ]"#);
        let e = r.unwrap_err();
        assert_eq!(e.msg, "syntax error, unexpected end of file");
        assert_eq!(ev.describe_pos(e.pos), "«string»:2:1");
        assert!(traces(&e).contains(&"while parsing the output from 'echo'"));
        let e = err(true, false, r#"builtins.exec [ "echo" "throw \"boom\"" ]"#);
        assert_eq!(e.msg, "boom");
        assert!(traces(&e).contains(&"while evaluating the output from 'echo'"));
    }

    #[test]
    fn context_must_be_in_the_store() {
        let sp = "/nix/store/00000000000000000000000000000000-x";
        let e = err(
            true,
            false,
            &format!(
                r#"builtins.exec [ (builtins.appendContext "{sp}" {{ "{sp}" = {{ path = true; }}; }}) ]"#
            ),
        );
        assert_eq!(
            e.msg,
            format!(
                "cannot execute '{sp}', since store path '00000000000000000000000000000000-x' is not valid"
            )
        );
        let drv = r#"(derivation { name = "p"; builder = "/bin/sh"; system = "x"; })"#;
        let e = err(true, false, &format!("builtins.exec [ {drv}.outPath ]"));
        assert_eq!(e.kind, ErrorKind::Unsupported, "{}", e.msg);
    }

    /// The read set cannot hold what a program prints, so the run is not
    /// written to the tracing cache.
    #[test]
    fn a_run_that_execs_is_uncacheable() {
        let (ev, r) = run(true, false, r#"builtins.exec [ "echo" "1" ]"#);
        assert_eq!(r.unwrap(), "1");
        assert!(!ev.trace().is_cacheable());
        let (ev, _) = run(true, false, "builtins ? exec");
        assert!(ev.trace().is_cacheable());
    }
}
