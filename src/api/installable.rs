//! One installable: the attribute paths a fragment names in a flake's
//! outputs, the values they select, and the derivations those values become
//! (docs/reference/plumbing.md).

use std::collections::HashSet;

use crate::api::build::Root;
use crate::eval::{Error, ErrorKind, EvalResult, Evaluator, Pos};
use crate::runtime::Ctx;
use crate::runtime::Word;
use crate::store::path::StorePath;

/// Which `getActualAttrPaths` table a fragment is tried against
/// (installable-flake.cc, with each command's own defaults and prefixes).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prefixes {
    /// `nix build`, `nix eval`: one package.
    Package,
    /// `nix develop`, `print-dev-env`: a development shell first.
    DevShell,
    /// `nix search`: the package sets, not one package.
    PackageSet,
}

impl Prefixes {
    pub fn parse(s: &str) -> Option<Prefixes> {
        match s {
            "package" => Some(Prefixes::Package),
            "dev-shell" => Some(Prefixes::DevShell),
            "package-set" => Some(Prefixes::PackageSet),
            _ => None,
        }
    }
}

/// How many candidates the caller wants: the first that is there, or every
/// one of them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Want {
    First,
    All,
}

/// A candidate that resolved: the value and the attribute path it was at.
pub struct Found {
    pub value: Word,
    pub path: Vec<String>,
}

/// The attribute paths `fragment` names, in order. A leading `.` makes the
/// rest the whole path; an empty fragment takes the defaults.
pub fn attr_paths(p: Prefixes, fragment: &str, system: &str) -> Vec<String> {
    if let Some(rest) = fragment.strip_prefix('.') {
        return vec![rest.to_string()];
    }
    if fragment.is_empty() {
        return match p {
            Prefixes::Package => vec![
                format!("packages.{system}.default"),
                format!("defaultPackage.{system}"),
            ],
            Prefixes::DevShell => vec![
                format!("devShells.{system}.default"),
                format!("devShell.{system}"),
                format!("packages.{system}.default"),
                format!("defaultPackage.{system}"),
            ],
            Prefixes::PackageSet => vec![
                format!("packages.{system}"),
                format!("legacyPackages.{system}"),
            ],
        };
    }
    let mut v = Vec::new();
    if p == Prefixes::DevShell {
        v.push(format!("devShells.{system}.{fragment}"));
    }
    v.push(format!("packages.{system}.{fragment}"));
    v.push(format!("legacyPackages.{system}.{fragment}"));
    v.push(fragment.to_string());
    v
}

/// installable-flake.cc:showAttrPaths.
pub fn show_attr_paths(paths: &[String]) -> String {
    let mut s = String::new();
    for (n, p) in paths.iter().enumerate() {
        if n > 0 {
            s.push_str(if n + 1 == paths.len() { " or " } else { ", " });
        }
        s.push('\'');
        s.push_str(p);
        s.push('\'');
    }
    s
}

/// The candidates of `fragment` selected from a flake's `outputs`. Never
/// empty: a miss is `MissingAttr` naming every candidate, as Nix reports it.
pub fn from_outputs(
    ev: &mut Evaluator,
    outputs: Word,
    loc: &str,
    prefixes: Prefixes,
    fragment: &str,
    want: Want,
) -> EvalResult<Vec<Found>> {
    let candidates = attr_paths(prefixes, fragment, ev.current_system());
    let mut found = Vec::new();
    for c in &candidates {
        let path = super::flake::parse_attr_path(c)
            .map_err(|m| Error::boxed(ErrorKind::Eval, m.to_string(), Pos::NONE))?;
        if let Some(value) = super::flake::select(ev, outputs, &path)? {
            found.push(Found { value, path });
            if want == Want::First {
                return Ok(found);
            }
        }
    }
    if found.is_empty() {
        return Err(Error::boxed(
            ErrorKind::MissingAttr,
            format!(
                "flake '{loc}' does not provide attribute {}",
                show_attr_paths(&candidates)
            ),
            Pos::NONE,
        ));
    }
    Ok(found)
}

/// attr-path.cc:findAlongAttrPath. Auto-calls at every step, selects an
/// attribute by name or a list element by index. Every command that takes
/// `-A` or an attr-path fragment goes through this, so a file whose top
/// level is a function is called with `--arg`/`--argstr` the same way
/// everywhere (docs/reference/commands.md).
pub fn find_along_attr_path(
    ev: &mut Evaluator,
    root: Word,
    attr_path: &str,
    args: Word,
) -> EvalResult<Word> {
    let mut v = root;
    let path = super::flake::parse_attr_path(attr_path)
        .map_err(|e| Error::boxed(ErrorKind::Parse, e.to_string(), Pos::NONE))?;
    for attr in path {
        v = ev.auto_call(v, args)?;
        v = ev.force(v)?;
        match attr.parse::<u32>() {
            Ok(idx) => {
                let Some(l) = v.as_list() else {
                    return Err(Error::boxed(
                        ErrorKind::Type,
                        format!(
                            "the expression selected by the selection path '{attr_path}' should be a list"
                        ),
                        Pos::NONE,
                    ));
                };
                let cells = l.cells();
                let Some(cell) = cells.get(idx as usize) else {
                    return Err(Error::boxed(
                        ErrorKind::MissingAttr,
                        format!("list index {idx} in selection path '{attr_path}' is out of range"),
                        Pos::NONE,
                    ));
                };
                v = cell.get();
            }
            Err(_) => {
                let Some(attrs) = v.as_attrs() else {
                    return Err(Error::boxed(
                        ErrorKind::Type,
                        format!(
                            "the expression selected by the selection path '{attr_path}' should be a set"
                        ),
                        Pos::NONE,
                    ));
                };
                if attr.is_empty() {
                    return Err(Error::boxed(
                        ErrorKind::Eval,
                        format!("empty attribute name in selection path '{attr_path}'"),
                        Pos::NONE,
                    ));
                }
                let sym = ev.sym(attr.as_bytes());
                let Some(a) = attrs.get(sym) else {
                    return Err(Error::boxed(
                        ErrorKind::MissingAttr,
                        format!("attribute '{attr}' in selection path '{attr_path}' not found"),
                        Pos::NONE,
                    ));
                };
                v = a.cell.get();
            }
        }
    }
    Ok(v)
}

fn parse_drv(p: &[u8]) -> EvalResult<StorePath> {
    StorePath::parse(&String::from_utf8_lossy(p))
        .map_err(|e| Error::boxed(ErrorKind::Eval, e.0, Pos::NONE))
}

fn forced_string(ev: &mut Evaluator, w: Word, ctx: &str) -> EvalResult<String> {
    let s = ev.force_string(w, Pos::NONE, ctx)?;
    Ok(String::from_utf8_lossy(s.bytes).into_owned())
}

/// A list of strings, `not_list` when the value is something else.
fn string_list(ev: &mut Evaluator, w: Word, not_list: &str, ctx: &str) -> EvalResult<Vec<String>> {
    let w = ev.force(w)?;
    let Some(l) = w.as_list() else {
        return Err(Error::boxed(ErrorKind::Type, not_list, Pos::NONE));
    };
    let cells: Vec<Word> = l.cells().iter().map(crate::runtime::Cell::get).collect();
    cells
        .into_iter()
        .map(|c| forced_string(ev, c, ctx))
        .collect()
}

/// `type = "derivation"`, the test both the get-drvs walk and the output
/// selection start from.
fn is_derivation(ev: &mut Evaluator, attrs: &'static crate::runtime::AttrsObj) -> EvalResult<bool> {
    let Some(t) = attrs.get(ev.s.type_) else {
        return Ok(false);
    };
    let t = forced_string(ev, t.cell.get(), "while evaluating the 'type' attribute")?;
    Ok(t == "derivation")
}

/// The `drvPath` of a derivation attrset.
fn drv_of(ev: &mut Evaluator, attrs: &'static crate::runtime::AttrsObj) -> EvalResult<StorePath> {
    let Some(dp) = attrs.get(ev.s.drv_path) else {
        return Err(Error::boxed(
            ErrorKind::MissingAttr,
            "derivation without a 'drvPath' attribute",
            Pos::NONE,
        ));
    };
    let dp = ev.force_string(dp.cell.get(), Pos::NONE, "while evaluating 'drvPath'")?;
    parse_drv(dp.bytes)
}

/// The `outputName` of a derivation attrset.
fn output_name(
    ev: &mut Evaluator,
    attrs: &'static crate::runtime::AttrsObj,
    drv: &StorePath,
) -> EvalResult<String> {
    let sym = ev.sym(b"outputName");
    let Some(on) = attrs.get(sym) else {
        return Err(Error::boxed(
            ErrorKind::MissingAttr,
            format!("derivation '{drv}' lacks an 'outputName' attribute"),
            Pos::NONE,
        ));
    };
    forced_string(ev, on.cell.get(), "while evaluating 'outputName'")
}

/// The derivations a build result names: an attrset with `drvPath`, a string
/// with derivation context, or a list of those. `!out!drv` context also names
/// the wanted output.
pub fn drv_roots(
    ev: &mut Evaluator,
    v: Word,
    roots: &mut Vec<(StorePath, Option<String>)>,
) -> EvalResult<()> {
    let bad = |msg: String| Error::boxed(ErrorKind::Eval, msg, Pos::NONE);
    let v = ev.force(v)?;
    if let Some(attrs) = v.as_attrs() {
        if attrs.get(ev.s.drv_path).is_none() {
            return Err(bad(
                "expression evaluated to an attrset without 'drvPath'".into()
            ));
        }
        let p = drv_of(ev, attrs)?;
        if !p.is_derivation() {
            return Err(bad(format!("'{p}' is not a derivation path")));
        }
        roots.push((p, None));
        return Ok(());
    }
    if let Some(s) = v.as_string() {
        let mut found = false;
        for c in s.ctx {
            match c {
                Ctx::Built { drv, output } => {
                    let (layers, root) = crate::runtime::split_built(drv);
                    if !layers.is_empty() {
                        return Err(bad(
                            "iets: the output of a dynamic derivation is not an installable"
                                .to_string(),
                        ));
                    }
                    let out = String::from_utf8_lossy(output).into_owned();
                    roots.push((parse_drv(root)?, Some(out)));
                    found = true;
                }
                Ctx::DrvDeep(drv) => {
                    roots.push((parse_drv(drv)?, None));
                    found = true;
                }
                Ctx::Opaque(_) | Ctx::Impure(_) => {}
            }
        }
        if !found {
            return Err(bad(format!(
                "string '{}' has no derivation in its context",
                String::from_utf8_lossy(s.bytes)
            )));
        }
        return Ok(());
    }
    if let Some(l) = v.as_list() {
        for c in l.cells() {
            drv_roots(ev, c.get(), roots)?;
        }
        return Ok(());
    }
    Err(bad(
        "expression must evaluate to a derivation, a string with derivation context, or a list of those".into(),
    ))
}

/// What `iets build` builds. A derivation, a string with derivation context
/// or a list is [`drv_roots`]. Any other attrset is a tree for
/// [`get_drvs_at`], entered at its member named after the current system when
/// it has one: that is how a flake lays out `checks` and `packages`.
pub fn roots_of(
    ev: &mut Evaluator,
    v: Word,
    args: Word,
    attr: &str,
    roots: &mut Vec<(StorePath, Option<String>)>,
) -> EvalResult<()> {
    let v = ev.force(v)?;
    let Some(tree) = v.as_attrs().filter(|a| a.get(ev.s.drv_path).is_none()) else {
        return drv_roots(ev, v, roots);
    };
    let system = ev.current_system();
    let sym = ev.sym(system.as_bytes());
    let (v, attr) = match tree.get(sym) {
        Some(a) => (a.cell.get(), join_attr(attr, system.as_bytes())),
        None => (v, attr.to_string()),
    };
    let mut found = Vec::new();
    get_drvs_at(ev, v, args, &mut found, &mut HashSet::new(), &attr)?;
    roots.extend(found.into_iter().map(|r| (r.drv, None)));
    Ok(())
}

/// get-drvs.cc:getDerivations. A derivation is one root; an attrset is walked
/// in lexicographic name order and recursed into only with
/// `recurseForDerivations = true`; a list is always walked.
pub fn get_drvs(
    ev: &mut Evaluator,
    v: Word,
    args: Word,
    out: &mut Vec<Root>,
    seen: &mut HashSet<(String, String)>,
) -> EvalResult<()> {
    get_drvs_at(ev, v, args, out, seen, "")
}

/// [`get_drvs`], telling every derivation it finds which attribute path it was
/// reached by (`attr` is the path already walked, from `-A`).
pub fn get_drvs_at(
    ev: &mut Evaluator,
    v: Word,
    args: Word,
    out: &mut Vec<Root>,
    seen: &mut HashSet<(String, String)>,
    attr: &str,
) -> EvalResult<()> {
    let v = ev.auto_call(v, args)?;
    let v = ev.force(v)?;
    if one_derivation(ev, v, out, seen, attr)? {
        return Ok(());
    }
    if let Some(attrs) = v.as_attrs() {
        let mut names: Vec<(Vec<u8>, Word)> = attrs
            .iter()
            .map(|a| (ev.interner.name(a.sym).to_vec(), a.cell.get()))
            .collect();
        names.sort_by(|a, b| a.0.cmp(&b.0));
        let recurse_sym = ev.sym(b"recurseForDerivations");
        for (name, child) in names {
            if !is_attr_path_component(&name) {
                continue;
            }
            let sub = join_attr(attr, &name);
            let child = ev.force(child)?;
            if one_derivation(ev, child, out, seen, &sub)? {
                continue;
            }
            let Some(ca) = child.as_attrs() else { continue };
            let Some(r) = ca.get(recurse_sym) else {
                continue;
            };
            if ev.force_bool(
                r.cell.get(),
                r.pos,
                "while evaluating the attribute `recurseForDerivations`",
            )? {
                get_drvs_at(ev, child, args, out, seen, &sub)?;
            }
        }
        return Ok(());
    }
    if let Some(l) = v.as_list() {
        for c in l.cells() {
            get_drvs_at(ev, c.get(), args, out, seen, attr)?;
        }
        return Ok(());
    }
    Err(Error::boxed(
        ErrorKind::Type,
        "expression does not evaluate to a derivation (or a set or list of those)",
        Pos::NONE,
    ))
}

/// `a.b` from `a` and `b`; `b` alone when the prefix is empty.
fn join_attr(prefix: &str, name: &[u8]) -> String {
    let name = String::from_utf8_lossy(name);
    if prefix.is_empty() {
        name.into_owned()
    } else {
        format!("{prefix}.{name}")
    }
}

/// True when `v` was a derivation and is now in `out`, or was a duplicate.
fn one_derivation(
    ev: &mut Evaluator,
    v: Word,
    out: &mut Vec<Root>,
    seen: &mut HashSet<(String, String)>,
    attr: &str,
) -> EvalResult<bool> {
    let Some(attrs) = v.as_attrs() else {
        return Ok(false);
    };
    if !is_derivation(ev, attrs)? {
        return Ok(false);
    }
    let drv = drv_of(ev, attrs)?;
    let output = output_name(ev, attrs, &drv)?;
    if output.is_empty() {
        return Err(Error::boxed(
            ErrorKind::Eval,
            format!("derivation '{drv}' lacks an 'outputName' attribute"),
            Pos::NONE,
        ));
    }
    ev.drv.set_attr(&drv, attr);
    if seen.insert((drv.to_string(), output.clone())) {
        out.push(Root { drv, output });
    }
    Ok(true)
}

/// get-drvs.cc:isAttrPathComponent: `[A-Za-z_][A-Za-z0-9-_+]*`.
fn is_attr_path_component(name: &[u8]) -> bool {
    let Some((first, rest)) = name.split_first() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || *first == b'_') {
        return false;
    }
    rest.iter()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'+'))
}

/// The program a flake output names, and the derivation output that holds it.
pub struct App {
    pub drv: StorePath,
    pub output: String,
    pub out_path: String,
    /// `<out_path>/bin/<mainProgram>`.
    pub program: String,
}

/// app.cc:InstallableValue::toApp for an output that must be a derivation, as
/// `nix fmt` requires of `formatter.<system>`: the program is
/// `outPath/bin/<mainProgram>`, and `mainProgram` is `meta.mainProgram`, else
/// `pname`, else `name` without its version (names.cc:DrvName).
pub fn to_app(ev: &mut Evaluator, v: Word, attr_path: &str) -> EvalResult<App> {
    let v = ev.force(v)?;
    let missing = |what: &str| {
        Error::boxed(
            ErrorKind::MissingAttr,
            format!("attribute '{attr_path}.{what}' missing"),
            Pos::NONE,
        )
    };
    let Some(attrs) = v.as_attrs() else {
        return Err(missing("type"));
    };
    if attrs.get(ev.s.type_).is_none() {
        return Err(missing("type"));
    }
    if !is_derivation(ev, attrs)? {
        return Err(Error::boxed(
            ErrorKind::Eval,
            format!("attribute '{attr_path}' should have type 'derivation'"),
            Pos::NONE,
        ));
    }
    let drv = drv_of(ev, attrs)?;
    let output = output_name(ev, attrs, &drv)?;
    let out_path = match attrs.get(ev.s.out_path) {
        Some(a) => forced_string(ev, a.cell.get(), "while evaluating 'outPath'")?,
        None => return Err(missing("outPath")),
    };
    let name = match attrs.get(ev.s.name) {
        Some(a) => forced_string(ev, a.cell.get(), "while evaluating 'name'")?,
        None => return Err(missing("name")),
    };
    let main = main_program(ev, attrs)?.unwrap_or_else(|| drv_pname(&name).to_string());
    Ok(App {
        drv,
        output,
        program: format!("{out_path}/bin/{main}"),
        out_path,
    })
}

/// names.cc:DrvName: the name runs up to the first `-` that is not followed
/// by a letter.
fn drv_pname(s: &str) -> &str {
    let b = s.as_bytes();
    match (0..b.len())
        .find(|&i| b[i] == b'-' && b.get(i + 1).is_some_and(|c| !c.is_ascii_alphabetic()))
    {
        Some(i) => &s[..i],
        None => s,
    }
}

/// `meta.mainProgram`, else `pname`; `None` when neither is there.
fn main_program(
    ev: &mut Evaluator,
    attrs: &'static crate::runtime::AttrsObj,
) -> EvalResult<Option<String>> {
    let meta_sym = ev.sym(b"meta");
    let main_sym = ev.sym(b"mainProgram");
    if let Some(m) = attrs.get(meta_sym) {
        let meta = ev.force(m.cell.get())?;
        if let Some(ma) = meta.as_attrs()
            && let Some(mp) = ma.get(main_sym)
        {
            return Ok(Some(forced_string(
                ev,
                mp.cell.get(),
                "while evaluating 'meta.mainProgram'",
            )?));
        }
    }
    let pname_sym = ev.sym(b"pname");
    match attrs.get(pname_sym) {
        Some(p) => Ok(Some(forced_string(
            ev,
            p.cell.get(),
            "while evaluating 'pname'",
        )?)),
        None => Ok(None),
    }
}

/// One installable's derivation and the outputs it installs.
pub struct Install {
    pub drv: StorePath,
    pub outputs: Vec<String>,
}

/// The derivation a value names, with no output selection
/// (installables.cc:Installable::toDerivations, which ignores the outputs).
/// `what` names the installable in the error a non-derivation gets.
pub fn to_derivation(ev: &mut Evaluator, v: Word, what: &str) -> EvalResult<StorePath> {
    let v = ev.force(v)?;
    let not_a_drv = || {
        Error::boxed(
            ErrorKind::Unsupported,
            format!("the installable '{what}' is not a derivation"),
            Pos::NONE,
        )
    };
    let Some(attrs) = v.as_attrs() else {
        return Err(not_a_drv());
    };
    if !is_derivation(ev, attrs)? {
        return Err(not_a_drv());
    }
    drv_of(ev, attrs)
}

/// The derivation a value names and its outputs to install
/// (get-drvs.cc:queryOutputs, installable-flake.cc:toDerivedPaths):
/// `outputSpecified` picks one, else `meta.outputsToInstall`, else every
/// output for an attr-path installable and `out` for a flake one. `what` names
/// the installable in the error a non-derivation gets.
pub fn to_install(ev: &mut Evaluator, v: Word, what: &str, flake: bool) -> EvalResult<Install> {
    let v = ev.force(v)?;
    let unsupported = |msg: String| Error::boxed(ErrorKind::Unsupported, msg, Pos::NONE);
    let not_a_drv = || unsupported(format!("the installable '{what}' is not a derivation"));
    let Some(attrs) = v.as_attrs() else {
        return Err(not_a_drv());
    };
    if !is_derivation(ev, attrs)? {
        return Err(not_a_drv());
    }
    let drv = drv_of(ev, attrs)?;

    let specified_sym = ev.sym(b"outputSpecified");
    let specified = match attrs.get(specified_sym) {
        Some(a) => ev.force_bool(
            a.cell.get(),
            a.pos,
            "while evaluating the 'outputSpecified' attribute of a derivation",
        )?,
        None => false,
    };
    if specified {
        let o = output_name(ev, attrs, &drv)?;
        return Ok(Install {
            drv,
            outputs: vec![o],
        });
    }
    let meta_sym = ev.sym(b"meta");
    if let Some(m) = attrs.get(meta_sym) {
        let meta = ev.force(m.cell.get())?;
        let to_install_sym = ev.sym(b"outputsToInstall");
        if let Some(ma) = meta.as_attrs()
            && let Some(oti) = ma.get(to_install_sym)
        {
            let mut outputs = string_list(
                ev,
                oti.cell.get(),
                "this derivation has bad 'meta.outputsToInstall'",
                "while evaluating 'meta.outputsToInstall'",
            )?;
            outputs.sort();
            return Ok(Install { drv, outputs });
        }
    }
    if flake {
        return Ok(Install {
            drv,
            outputs: vec!["out".into()],
        });
    }
    let outputs_sym = ev.sym(b"outputs");
    let mut outputs = match attrs.get(outputs_sym) {
        Some(o) => string_list(
            ev,
            o.cell.get(),
            "the 'outputs' attribute of a derivation is not a list",
            "while evaluating the name of an output of a derivation",
        )?,
        None => Vec::new(),
    };
    if outputs.is_empty() {
        outputs.push("out".into());
    }
    outputs.sort();
    Ok(Install { drv, outputs })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_a_program_takes_from_a_derivation_name() {
        assert_eq!(drv_pname("treefmt-2.0.0"), "treefmt");
        assert_eq!(drv_pname("nixfmt"), "nixfmt");
        // a dash before a letter is part of the name
        assert_eq!(drv_pname("nixfmt-tree"), "nixfmt-tree");
        assert_eq!(drv_pname("apache-httpd-2.0.48"), "apache-httpd");
        assert_eq!(drv_pname("fmt-e2e-1.0"), "fmt-e2e");
        assert_eq!(drv_pname("x-"), "x-");
    }

    #[test]
    fn default_and_fragment_attr_paths() {
        assert_eq!(
            attr_paths(Prefixes::DevShell, "", "x86_64-linux"),
            [
                "devShells.x86_64-linux.default",
                "devShell.x86_64-linux",
                "packages.x86_64-linux.default",
                "defaultPackage.x86_64-linux"
            ]
        );
        assert_eq!(
            attr_paths(Prefixes::DevShell, "ci", "x86_64-linux"),
            [
                "devShells.x86_64-linux.ci",
                "packages.x86_64-linux.ci",
                "legacyPackages.x86_64-linux.ci",
                "ci"
            ]
        );
        assert_eq!(
            attr_paths(Prefixes::Package, "", "s"),
            ["packages.s.default", "defaultPackage.s"]
        );
        assert_eq!(
            attr_paths(Prefixes::Package, "hello", "s"),
            ["packages.s.hello", "legacyPackages.s.hello", "hello"]
        );
        assert_eq!(
            attr_paths(Prefixes::PackageSet, "", "s"),
            ["packages.s", "legacyPackages.s"]
        );
        assert_eq!(
            attr_paths(Prefixes::PackageSet, "sub", "s"),
            ["packages.s.sub", "legacyPackages.s.sub", "sub"]
        );
        for p in [Prefixes::Package, Prefixes::DevShell, Prefixes::PackageSet] {
            assert_eq!(attr_paths(p, ".a.b", "s"), ["a.b"]);
        }
        assert_eq!(
            show_attr_paths(&["a".into(), "b".into(), "c".into()]),
            "'a', 'b' or 'c'"
        );
    }
}
