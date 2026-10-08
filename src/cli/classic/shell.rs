//! `nix-shell` (src/nix/nix-build/nix-build.cc, the `runEnv` path): build the
//! derivation's *inputs*, then run bash on a rc file that sources
//! `$stdenv/setup` with the derivation's environment exported. This is not
//! `print-dev-env`; nothing here builds the `-env` derivation.
//! See `docs/reference/classic.md`.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::{env, fs, slice};

use super::{Argv, Common, common_flag, eval_config, fallback, lookup_file_arg, shell_settings};
use crate::api::build::Root;
use crate::api::{build as apibuild, installable};
use crate::build::backend::Worker;
use crate::build::upload::{PlanUpload, Uploader};
use crate::builtins::derivation;
use crate::cli::{self, Outcome, Run, auto_args, cwd, devenv, report};
use crate::compile::ir::ParamIr;
use crate::devenv::escape_shell_arg_always as esc;
use crate::error::{self, Error as CrateError};
use crate::eval::{Error, ErrorKind, Evaluator, Pos};
use crate::remote::event::{self, Event};
use crate::remote::plan::Plan;
use crate::remote::worker::{Options, Transport};
use crate::runtime::{Value, Word};
use crate::store::drv::{Derivation, DerivationOutput};
use crate::store::path::{STORE_DIR, StorePath, hash_placeholder};
use crate::syntax::symbol::Sym;
use crate::{cachefs, log};
use serde_json::Value as Json;

const NAME: &str = "nix-shell";

/// nix-build.cc: the environment variables `--pure` keeps, plus
/// `networkProxyVariables`. Nix means to keep the upper-case proxy variables as
/// well, but `getAllVariables` transforms into an empty string, so only these
/// lower-case names survive; iets keeps the same set.
const KEEP_VARS: &[&str] = &[
    "HOME",
    "XDG_RUNTIME_DIR",
    "USER",
    "LOGNAME",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "WAYLAND_SOCKET",
    "PATH",
    "TERM",
    "IN_NIX_SHELL",
    "NIX_SHELL_PRESERVE_PROMPT",
    "TZ",
    "PAGER",
    "NIX_BUILD_SHELL",
    "SHLVL",
    "http_proxy",
    "https_proxy",
    "ftp_proxy",
    "all_proxy",
    "no_proxy",
];

#[derive(Default)]
struct Shell {
    c: Common,
    pure: bool,
    /// `--run`/`--command`, with the `exit` nix-build.cc appends.
    env_command: String,
    /// bash gets `--rcfile` only when it will be interactive.
    interactive: bool,
    /// `--keep NAME`, on top of `KEEP_VARS`.
    keep: Vec<String>,
}

fn parse(argv: &[String]) -> Result<Shell, String> {
    let mut s = Shell {
        // Same condition as bash uses for interactive shells.
        interactive: unsafe { libc::isatty(0) == 1 && libc::isatty(2) == 1 },
        ..Shell::default()
    };
    let mut a = Argv { all: argv, i: 0 };
    while a.i < argv.len() {
        let f = argv[a.i].clone();
        match f.as_str() {
            "--run" | "--command" => {
                let v = a.value(&f)?;
                if f == "--run" {
                    s.interactive = false;
                }
                s.env_command = format!("{v}\nexit");
            }
            "--pure" => s.pure = true,
            "--impure" => s.pure = false,
            "--keep" => {
                let v = a.value(&f)?;
                s.keep.push(v);
            }
            // no-ops in Nix too: nix-build.cc calls the first two obsolete, and
            // only nix-instantiate has a GC warning
            "--add-drv-link" | "--indirect" | "--no-gc-warning" => {}
            "-Q" | "--no-build-output" => s.c.quiet = true,
            "-p" | "--packages" | "-i" | "-" | "--exclude" | "--dry-run" | "--check"
            | "--repair" | "-o" | "--out-link" | "--no-out-link" | "--no-link" => {
                return Err(format!("'{f}' is not implemented"));
            }
            _ => {
                if !common_flag(&mut s.c, &mut a, &f)? {
                    if f.starts_with('-') {
                        return Err(format!("'{f}' is not implemented"));
                    }
                    s.c.files.push(f);
                }
            }
        }
        a.i += 1;
    }
    if s.c.flags.attrs.is_empty() {
        s.c.flags.attrs.push(String::new());
    }
    Ok(s)
}

/// nix-build.cc reads `argv[1]` and treats it as a `#!` script when it starts
/// with `#!`; that whole mode (`-i INTERPRETER`) belongs to the real nix-shell.
fn is_shebang(argv: &[String]) -> bool {
    let Some(first) = argv.first() else {
        return false;
    };
    fs::read(first).is_ok_and(|b| b.starts_with(b"#!"))
}

pub fn main(argv: &[String]) -> i32 {
    if is_shebang(argv) {
        fallback(NAME, argv, "a '#!' script");
    }
    let mut s = match parse(argv) {
        Ok(s) => s,
        Err(reason) => fallback(NAME, argv, &reason),
    };
    if let Err(reason) = default_file(&mut s) {
        return CrateError::Usage(reason).report();
    }
    let (cfg, transport, options) = match shell_settings(&s.c) {
        Ok(t) => t,
        Err(reason) => fallback(NAME, argv, &reason),
    };
    let always = cfg.bool("nix-shell-always-looks-for-shell-nix");
    if always.ok().flatten() == Some(false) {
        let why = "'nix-shell-always-looks-for-shell-nix = false' is not implemented";
        fallback(NAME, argv, why);
    }
    // nix-build.cc sets this before it evaluates, so `builtins.getEnv` sees it
    // and the child inherits it.
    unsafe {
        env::set_var("IN_NIX_SHELL", if s.pure { "pure" } else { "impure" });
    }
    let ec = eval_config(&s.c, &cfg);
    let s = Arc::new(s);
    let run = Arc::clone(&s);
    let cores = match cfg.cores() {
        Ok(c) => c,
        Err(e) => return e.report(),
    };
    match cli::run_reported(Run::on(ec), move |ev, _| prepare(ev, &run)) {
        Outcome::Failed(code) => code,
        Outcome::Unsupported(reason) => fallback(NAME, argv, &reason),
        Outcome::Ok(r) => realise(&s, &transport, &options, *r, cores),
    }
}

/// nix-build.cc: with no file argument, `shell.nix` then `default.nix`, and a
/// clearer error than the evaluator's when neither is there.
fn default_file(s: &mut Shell) -> Result<(), String> {
    if s.c.from_args || !s.c.files.is_empty() {
        return Ok(());
    }
    if !Path::new("shell.nix").exists() && !Path::new("default.nix").exists() {
        return Err(
            "no argument specified and no 'shell.nix' or 'default.nix' file found in the working directory"
                .into(),
        );
    }
    s.c.files.push(".".into());
    Ok(())
}

/// nix-build.cc:resolveShellExprPath: a directory resolves to `shell.nix`, not
/// to `default.nix`, when it has one.
fn resolve_shell_expr_path(p: &[u8]) -> Result<Vec<u8>, String> {
    let path = PathBuf::from(OsStr::from_bytes(p));
    if !path.is_dir() {
        return Ok(p.to_vec());
    }
    for name in ["shell.nix", "default.nix"] {
        let c = path.join(name);
        if c.exists() {
            return Ok(c.into_os_string().into_encoded_bytes());
        }
    }
    Err(format!(
        "neither 'shell.nix' nor 'default.nix' found in '{}'",
        path.display()
    ))
}

/// What the evaluation produced: the shell derivation, the bash to run, and the
/// closure to upload before building the derivation's inputs.
struct Ready {
    drv_path: StorePath,
    drv: Derivation,
    bash: Bash,
    plan: Plan,
    /// What the user asked for, resolved: the files (or expressions) and the
    /// attribute paths. It names this project's roots ([`keep_inputs`]).
    key: Vec<Vec<u8>>,
}

enum Bash {
    /// `$NIX_BUILD_SHELL`, or `bash` from `PATH` when nixpkgs has no
    /// `bashInteractive` to evaluate.
    Path(String),
    /// `(import <nixpkgs> {}).bashInteractive`, still to be built.
    Drv(StorePath),
}

fn prepare(ev: &mut Evaluator, s: &Shell) -> error::Result<Box<Ready>> {
    let cwd = cwd();
    let mut roots: Vec<Root> = Vec::new();
    let args = auto_args(ev, &s.c.flags.args, &cwd)?;
    let mut seen = HashSet::new();
    let mut key: Vec<Vec<u8>> = vec![b"nix-shell".to_vec()];
    for file in &s.c.files {
        let root = if s.c.from_args {
            key.push(file.as_bytes().to_vec());
            ev.eval_string(file, &cwd)?
        } else {
            let p = lookup_file_arg(ev, file, &cwd)?;
            let p = resolve_shell_expr_path(&p)
                .map_err(|m| Error::boxed(ErrorKind::Eval, m, Pos::NONE))?;
            key.push(p.clone());
            ev.eval_file(&p, Pos::NONE)?
        };
        let root = ev.force(root)?;
        let args = if takes_in_nix_shell(ev, root) {
            with_in_nix_shell(ev, args)?
        } else {
            args
        };
        for attr in &s.c.flags.attrs {
            let v = installable::find_along_attr_path(ev, root, attr, args)?;
            let v = ev.force(v)?;
            let args = if takes_in_nix_shell(ev, v) {
                with_in_nix_shell(ev, args)?
            } else {
                args
            };
            installable::get_drvs_at(ev, v, args, &mut roots, &mut seen, attr)?;
        }
    }
    let bash = bash(ev, &cwd);
    if roots.len() != 1 {
        return Err(CrateError::Usage(
            "nix-shell requires a single derivation".into(),
        ));
    }
    let drv = derivation::load_drv(ev, &roots[0].drv)?;
    if !drv
        .outputs
        .values()
        .all(|o| matches!(o, DerivationOutput::InputAddressed(_)))
    {
        return Err(CrateError::Unsupported(format!(
            "the derivation '{}' has to be resolved first",
            roots[0].drv
        )));
    }
    let mut targets = vec![roots[0].clone()];
    if let Bash::Drv(p) = &bash {
        targets.push(Root {
            drv: p.clone(),
            output: "out".into(),
        });
    }
    key.extend(s.c.flags.attrs.iter().map(|a| a.as_bytes().to_vec()));
    let plan = apibuild::plan(ev, &Root::paths(&targets))?;
    Ok(Box::new(Ready {
        drv_path: roots[0].drv.clone(),
        drv,
        bash,
        plan,
        key,
    }))
}

/// nix-build.cc: `$NIX_BUILD_SHELL`, else `(import <nixpkgs> {}).bashInteractive`;
/// any evaluation error means bash from the environment.
fn bash(ev: &mut Evaluator, cwd: &[u8]) -> Bash {
    if let Some(sh) = env::var_os("NIX_BUILD_SHELL") {
        return Bash::Path(sh.to_string_lossy().into_owned());
    }
    let mut roots = Vec::new();
    let r = (|| -> Result<(), Box<Error>> {
        let v = ev.eval_string("(import <nixpkgs> {}).bashInteractive", cwd)?;
        let empty = ev.mk_attrs(&mut Vec::new(), Pos::NONE)?;
        installable::get_drvs(ev, v, empty, &mut roots, &mut HashSet::new())
    })();
    match r {
        Ok(()) if roots.len() == 1 => Bash::Drv(roots[0].drv.clone()),
        Ok(()) => {
            let msg =
                "the 'bashInteractive' attribute in <nixpkgs> did not evaluate to a derivation";
            CrateError::Eval(Error::boxed(ErrorKind::Type, msg, Pos::NONE)).report();
            log::line("uses bash from your environment");
            Bash::Path("bash".into())
        }
        Err(e) => {
            report(ev, &e);
            log::line("uses bash from your environment");
            Bash::Path("bash".into())
        }
    }
}

/// nix-build.cc:takesNixShellAttr: a function with an `inNixShell` formal gets
/// it passed as `true`.
fn takes_in_nix_shell(ev: &Evaluator, w: Word) -> bool {
    let Value::Lambda(l) = w.view() else {
        return false;
    };
    let ParamIr::Formals { formals, .. } = &l.lam.param else {
        return false;
    };
    formals
        .iter()
        .any(|f| ev.interner.name(f.sym) == b"inNixShell")
}

fn with_in_nix_shell(ev: &mut Evaluator, args: Word) -> Result<Word, Box<Error>> {
    let mut entries: Vec<(Sym, Pos, Word)> = args
        .as_attrs()
        .map(|a| a.iter().map(|x| (x.sym, x.pos, x.cell.get())).collect())
        .unwrap_or_default();
    entries.push((ev.sym(b"inNixShell"), Pos::NONE, Word::TRUE));
    ev.mk_attrs(&mut entries, Pos::NONE)
}

/// Uploads the closure, builds the derivation's inputs (and bash), writes the rc
/// file and execs bash. Returns only on failure.
fn realise(s: &Shell, transport: &Transport, options: &Options, r: Ready, cores: u64) -> i32 {
    let quiet = s.c.quiet;
    let failed = Cell::new(false);
    let mut on_event = |e: event::Event| {
        match e {
            // under internal-json a line of a build is already its result record
            Event::Log(l) if !quiet && (l.drv_path.is_empty() || !log::json()) => {
                log::line(&String::from_utf8_lossy(&l.line))
            }
            Event::Failed(f) => {
                failed.set(true);
                log::line(&format!(
                    "error: build of '{}' failed: {}",
                    f.drv_path, f.reason
                ));
            }
            _ => {}
        }
    };
    let mut w = match Worker::open(transport, options, &mut on_event) {
        Ok(c) => c,
        Err(e) => return e.report(),
    };
    // nix-shell realises the derivation's inputs, never the derivation itself.
    let mut wanted: Vec<String> = r
        .drv
        .input_drvs
        .iter()
        .filter(|(_, node)| !node.outputs.is_empty())
        .map(|(p, node)| {
            format!(
                "{p}!{}",
                node.outputs.iter().cloned().collect::<Vec<_>>().join(",")
            )
        })
        .collect();
    wanted.extend(r.drv.input_srcs.iter().map(StorePath::to_string));
    if let Bash::Drv(p) = &r.bash {
        wanted.push(format!("{p}!out"));
    }
    let upload = PlanUpload::new(&r.plan, &apibuild::locate_store).upload(
        &mut w,
        slice::from_ref(&r.drv_path),
        &mut on_event,
    );
    if let Err(e) = upload {
        return e.report();
    }
    match w.conn.build_paths_with_results(&wanted, &mut on_event) {
        Ok(results) => {
            for b in &results {
                if !b.success() {
                    let code = log::build_exit(b.status, &b.msg);
                    failed.set(true);
                    let reason = format!("status {}: {}", b.status, b.msg);
                    let text = format!("error: build failed: {reason}");
                    log::build_failed(None, &reason, code, Some(&text));
                }
            }
        }
        Err(e) => return e.report(),
    }
    if failed.get() {
        return 1;
    }
    let shell = match &r.bash {
        Bash::Path(p) => p.clone(),
        Bash::Drv(p) => match w
            .conn
            .query_derivation_output_map(&p.to_string(), &mut on_event)
        {
            Ok(m) => match m.get("out").cloned().flatten() {
                Some(out) => format!("{out}/bin/bash"),
                None => {
                    let e = format!("derivation '{p}' has no output 'out'");
                    return CrateError::Store(e).report();
                }
            },
            Err(e) => return e.report(),
        },
    };
    keep_inputs(&mut w, &r, &shell, transport, &mut on_event);
    drop(w);
    match exec(s, &r.drv, &shell, cores) {
        Ok(()) => unreachable!("exec replaces the process"),
        Err(e) => e.report(),
    }
}

/// Keeps what this entry realised alive, so that the next entry rebuilds
/// nothing. `nix-shell` builds the derivation's inputs and not the derivation,
/// and no store path holds them all, so iets's roots store gets one link each.
/// The links are keyed on what the user asked for and the path's place in the
/// sorted list: a project replaces its own set in place, and the TTL sweep takes
/// what a set that shrank leaves. Best effort and silent, as in
/// [`crate::cli::devenv::keep_root`].
fn keep_inputs(
    w: &mut Worker,
    r: &Ready,
    shell: &str,
    transport: &Transport,
    on_event: &mut dyn FnMut(event::Event),
) {
    if !devenv::shell_roots(transport) {
        return;
    }
    let key: Vec<&[u8]> = r.key.iter().map(|p| p.as_slice()).collect();
    let mut paths: BTreeSet<String> = r.drv.input_srcs.iter().map(StorePath::to_string).collect();
    for (drv, node) in &r.drv.input_drvs {
        if node.outputs.is_empty() {
            continue;
        }
        let Ok(map) = w
            .conn
            .query_derivation_output_map(&drv.to_string(), on_event)
        else {
            return;
        };
        paths.extend(
            node.outputs
                .iter()
                .filter_map(|o| map.get(o).cloned().flatten()),
        );
    }
    // `shell` is `<output>/bin/bash` when nixpkgs built the bash
    if matches!(r.bash, Bash::Drv(_))
        && let Some(out) = Path::new(shell).parent().and_then(Path::parent)
    {
        paths.insert(out.to_string_lossy().into_owned());
    }
    for (i, p) in paths.iter().enumerate() {
        let index = i.to_string();
        let mut parts = key.clone();
        parts.push(index.as_bytes());
        let Some(link) = cachefs::root_link(None, &parts) else {
            return;
        };
        devenv::keep_root(transport, &link, p, Some(&mut w.conn));
    }
}

/// nix-build.cc's `runEnv` tail: the environment, the rc file, `execvp`.
fn exec(s: &Shell, drv: &Derivation, shell: &str, cores: u64) -> error::Result<()> {
    let tmp_dir = env::temp_dir().join(format!(
        "nix-shell-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos())
    ));
    fs::create_dir_all(&tmp_dir)
        .map_err(|e| CrateError::io(format!("cannot create '{}'", tmp_dir.display()), e))?;
    let tmp = tmp_dir.to_string_lossy().into_owned();

    let mut env: BTreeMap<OsString, OsString> = env::vars_os().collect();
    if s.pure {
        let keep: BTreeSet<&OsStr> = KEEP_VARS
            .iter()
            .map(OsStr::new)
            .chain(s.keep.iter().map(OsStr::new))
            .collect();
        env.retain(|k, _| keep.contains(k.as_os_str()));
        // NixOS hack: prevent /etc/bashrc from sourcing /etc/profile.
        env.insert("__ETC_PROFILE_SOURCED".into(), "1".into());
    }
    for v in ["NIX_BUILD_TOP", "TMPDIR", "TEMPDIR", "TMP", "TEMP"] {
        env.insert(v.into(), tmp.clone().into());
    }
    env.insert("NIX_STORE".into(), STORE_DIR.into());
    env.insert("NIX_BUILD_CORES".into(), cores.to_string().into());

    let pass_as_file: BTreeSet<&str> = match drv.structured_attrs {
        Some(_) => BTreeSet::new(),
        None => drv
            .env_str("passAsFile")
            .unwrap_or_default()
            .split_whitespace()
            .collect(),
    };
    let mut file_nr = 0;
    for (name, value) in &drv.env {
        if pass_as_file.contains(name.as_str()) {
            let p = tmp_dir.join(format!(".attr-{file_nr}"));
            file_nr += 1;
            write(&p, value)?;
            env.insert(format!("{name}Path").into(), p.into_os_string());
        } else {
            env.insert(name.into(), OsStr::from_bytes(value).to_owned());
        }
    }

    let mut structured_rc = String::new();
    if let Some(sa) = &drv.structured_attrs {
        let json = prepare_structured_attrs(sa, &drv.outputs)?;
        structured_rc = write_shell(&json);
        let sh = tmp_dir.join(".attrs.sh");
        let js = tmp_dir.join(".attrs.json");
        write(&js, Json::Object(json).to_string())?;
        write(&sh, &structured_rc)?;
        env.insert("NIX_ATTRS_SH_FILE".into(), sh.into_os_string());
        env.insert("NIX_ATTRS_JSON_FILE".into(), js.into_os_string());
    }

    let rc_path = tmp_dir.join("rc");
    write(&rc_path, rc_script(s, shell, &tmp, &structured_rc))?;

    let mut cmd = Command::new(shell);
    cmd.arg0("bash");
    if s.interactive {
        cmd.arg("--rcfile");
    }
    cmd.arg(&rc_path);
    cmd.env_clear().envs(env);
    let e = cmd.exec();
    Err(CrateError::io(format!("executing shell '{shell}'"), e))
}

fn write(path: &Path, text: impl AsRef<[u8]>) -> error::Result<()> {
    fs::write(path, text)
        .map_err(|e| CrateError::io(format!("cannot write '{}'", path.display()), e))
}

/// nix-build.cc's rc file, piece for piece.
fn rc_script(s: &Shell, shell: &str, tmp: &str, structured_rc: &str) -> String {
    let bin = Path::new(shell)
        .parent()
        .unwrap_or(Path::new("/"))
        .to_string_lossy()
        .into_owned();
    let mut rc = String::new();
    rc.push_str(&format!(
        "_nix_shell_clean_tmpdir() {{ command rm -rf {}; }};",
        esc(tmp)
    ));
    rc.push_str("trap _nix_shell_clean_tmpdir EXIT; ");
    rc.push_str("exitHooks+=(_nix_shell_clean_tmpdir); ");
    rc.push_str("failureHooks+=(_nix_shell_clean_tmpdir); ");
    if !s.pure {
        rc.push_str("[ -n \"$PS1\" ] && [ -e ~/.bashrc ] && source ~/.bashrc;");
        rc.push_str("p=$PATH; ");
    }
    // Always clear PATH; an impure shell rehydrates it from $p below.
    rc.push_str("unset PATH;");
    rc.push_str("dontAddDisableDepTrack=1;\n");
    rc.push_str(structured_rc);
    rc.push_str("\n[ -e $stdenv/setup ] && source $stdenv/setup; ");
    if !s.pure {
        rc.push_str("PATH=$PATH:$p; unset p; ");
    }
    rc.push_str(&format!("PATH={}:\"$PATH\"; ", esc(&bin)));
    rc.push_str(&format!("SHELL={}; ", esc(shell)));
    rc.push_str(&format!("BASH={}; ", esc(shell)));
    rc.push_str("set +e; ");
    rc.push_str("[ -n \"$PS1\" -a -z \"$NIX_SHELL_PRESERVE_PROMPT\" ] && ");
    let colour = if unsafe { libc::getuid() } == 0 {
        "1;31"
    } else {
        "1;32"
    };
    rc.push_str(&format!(
        "PS1='\\n\\[\\033[{colour}m\\][nix-shell:\\w]\\$\\[\\033[0m\\] '; "
    ));
    rc.push_str("if [ \"$(type -t runHook)\" = function ]; then runHook shellHook; fi; ");
    rc.push_str("unset NIX_ENFORCE_PURITY; ");
    rc.push_str("shopt -u nullglob; ");
    rc.push_str("unset TZ; ");
    if let Some(tz) = env::var_os("TZ") {
        rc.push_str(&format!("export TZ={}; ", esc(&tz.to_string_lossy())));
    }
    rc.push_str("shopt -s execfail;");
    rc.push_str(&s.env_command);
    rc
}

/// parsed-derivations.cc:StructuredAttrs::prepareStructuredAttrs. `outputs`
/// becomes the output placeholders; `exportReferencesGraph` would need path
/// info and closures, so it falls back to the real nix-shell.
fn prepare_structured_attrs(
    sa: &str,
    outputs: &BTreeMap<String, DerivationOutput>,
) -> error::Result<serde_json::Map<String, Json>> {
    let mut json: serde_json::Map<String, Json> = serde_json::from_str(sa)
        .map_err(|e| CrateError::Store(format!("structured attrs are not valid JSON: {e}")))?;
    let exports = match json.get("exportReferencesGraph") {
        None | Some(Json::Null) => false,
        Some(Json::Object(o)) => !o.is_empty(),
        Some(_) => true,
    };
    if exports {
        return Err(CrateError::Usage(
            "'exportReferencesGraph' in a nix-shell derivation is not implemented".into(),
        ));
    }
    json.insert(
        "outputs".into(),
        Json::Object(
            outputs
                .keys()
                .map(|n| (n.clone(), Json::String(hash_placeholder(n))))
                .collect(),
        ),
    );
    Ok(json)
}

/// parsed-derivations.cc:StructuredAttrs::writeShell: the `declare` lines for
/// every attribute whose value bash can hold.
fn write_shell(json: &serde_json::Map<String, Json>) -> String {
    let mut out = String::new();
    for (key, value) in json {
        if !is_sh_var_name(key) {
            continue;
        }
        if let Some(s) = simple_type(value) {
            out.push_str(&format!("declare {key}={s}\n"));
        } else if let Some(a) = value.as_array() {
            let mut s2 = String::new();
            if a.iter().all(|v| match simple_type(v) {
                Some(s) => {
                    s2.push_str(&s);
                    s2.push(' ');
                    true
                }
                None => false,
            }) {
                out.push_str(&format!("declare -a {key}=({s2})\n"));
            }
        } else if let Some(o) = value.as_object() {
            let mut s2 = String::new();
            if o.iter().all(|(k2, v2)| match simple_type(v2) {
                Some(s) => {
                    s2.push_str(&format!("[{}]={s} ", esc(k2)));
                    true
                }
                None => false,
            }) {
                out.push_str(&format!("declare -A {key}=({s2})\n"));
            }
        }
    }
    out
}

/// parsed-derivations.cc:shVarName, `[A-Za-z_][A-Za-z0-9_]*` matched whole.
fn is_sh_var_name(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `handleSimpleType`: a string, an integral number, `null` or a bool.
fn simple_type(v: &Json) -> Option<String> {
    match v {
        Json::String(s) => Some(esc(s)),
        Json::Number(n) => {
            let f = n.as_f64()? as f32;
            let i = n.as_f64()? as i32;
            (f.ceil() == f).then(|| i.to_string())
        }
        Json::Null => Some("''".into()),
        Json::Bool(b) => Some(if *b { "1" } else { "" }.into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sh_var_names() {
        assert!(is_sh_var_name("a_B9"));
        assert!(is_sh_var_name("_"));
        assert!(!is_sh_var_name("9a"));
        assert!(!is_sh_var_name("a-b"));
        assert!(!is_sh_var_name(""));
    }

    #[test]
    fn declares_for_every_bash_shaped_value() {
        let json: Json = serde_json::from_str(
            r#"{"name":"x","doCheck":true,"off":false,"n":3,"f":1.5,"nil":null,
                "l":["a","b"],"bad":[{"x":1}],"m":{"k":"v"},"not a name":1,
                "deep":{"k":[1]}}"#,
        )
        .unwrap();
        let s = write_shell(json.as_object().unwrap());
        assert_eq!(
            s,
            concat!(
                "declare doCheck=1\n",
                "declare -a l=('a' 'b' )\n",
                "declare -A m=(['k']='v' )\n",
                "declare n=3\n",
                "declare name='x'\n",
                "declare nil=''\n",
                "declare off=\n",
            )
        );
    }

    #[test]
    fn outputs_become_placeholders() {
        let mut outputs = BTreeMap::new();
        outputs.insert("out".to_string(), DerivationOutput::Deferred);
        let json = prepare_structured_attrs(r#"{"name":"x"}"#, &outputs).unwrap();
        assert_eq!(
            json["outputs"]["out"].as_str().unwrap(),
            hash_placeholder("out")
        );
        assert!(
            prepare_structured_attrs(
                r#"{"exportReferencesGraph":{"a":["/nix/store/x"]}}"#,
                &outputs
            )
            .is_err()
        );
    }
}
