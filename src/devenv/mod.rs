//! `iets print-dev-env`: the shell environment of a derivation, computed as
//! `nix print-dev-env` (src/nix/develop.cc, Nix 2.34.8) computes it. This module
//! holds the pure parts: the `-env` derivation, the JSON `get-env.sh` writes,
//! and the bash `makeRcScript` prints. The daemon flow lives in `main.rs`.
//! See docs/reference/commands.md.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::store::drv::{
    Derivation, DerivationOutput, DrvHashCache, DrvLoader, finalize_input_addressed_outputs,
};
use crate::store::hash::{HashAlgo, hash_bytes};
use crate::store::path::{StorePath, make_text_path};

/// src/nix/get-env.sh, verbatim: the builder script of the `-env` derivation. Nix embeds
/// the file through `get-env.sh.gen.hh`, a raw string literal whose opening delimiter is
/// followed by a newline, so the text Nix adds to the store starts with one.
pub const GET_ENV_SH: &str = concat!("\n", include_str!("get-env.sh"));

/// develop.cc:Common::ignoreVars.
pub const IGNORE_VARS: &[&str] = &[
    "BASHOPTS",
    "HOME",
    "NIX_BUILD_TOP",
    "NIX_ENFORCE_PURITY",
    "NIX_LOG_FD",
    "NIX_REMOTE",
    "PPID",
    "SHELLOPTS",
    "SSL_CERT_FILE",
    "TEMP",
    "TEMPDIR",
    "TERM",
    "TMP",
    "TMPDIR",
    "TZ",
    "UID",
];

/// develop.cc:makeRcScript savedVars: prepended to, not overwritten.
const SAVED_VARS: &[&str] = &["PATH", "XDG_DATA_DIRS"];

/// The store path of `get-env.sh` as a text path without references
/// (develop.cc: `addToStoreFromDump(..., Flat, Text, SHA256, {})`).
pub fn get_env_sh_path() -> StorePath {
    make_text_path(
        "get-env.sh",
        &hash_bytes(HashAlgo::Sha256, GET_ENV_SH.as_bytes()),
        &BTreeSet::new(),
    )
    .expect("get-env.sh is a valid store path name")
}

/// Mirrors develop.cc:getDerivationEnvironment up to `writeDerivation`: the same
/// inputs and nearly the same environment as `shell`, with `get-env.sh` as the only
/// argument, so the build dumps the environment instead of building. Output paths are
/// recomputed from the new name and arguments.
pub fn env_derivation(
    shell: &Derivation,
    cache: &mut DrvHashCache,
    loader: &mut DrvLoader<'_>,
) -> Result<Derivation> {
    let builder = shell.builder.rsplit('/').next().unwrap_or("");
    if builder != "bash" {
        return Err(Error::Store(
            "'nix develop' only works on derivations that use 'bash' as their builder".into(),
        ));
    }
    let mut drv = shell.clone();
    let get_env = get_env_sh_path();
    drv.args = vec![get_env.to_string()];
    if let Some(sa) = &drv.structured_attrs {
        drv.structured_attrs = Some(erase_output_checks(sa)?);
    } else {
        for k in [
            "allowedReferences",
            "allowedRequisites",
            "disallowedReferences",
            "disallowedRequisites",
        ] {
            drv.env_remove(k);
        }
    }
    drv.name.push_str("-env");
    let name = drv.name.clone();
    drv.env_set("name", name);
    drv.input_srcs.insert(get_env);
    let reset: Vec<String> = drv
        .outputs
        .iter()
        .filter(|(_, o)| {
            matches!(
                o,
                DerivationOutput::InputAddressed(_) | DerivationOutput::Fixed { .. }
            )
        })
        .map(|(n, _)| n.clone())
        .collect();
    for n in reset {
        drv.outputs.insert(n.clone(), DerivationOutput::Deferred);
        drv.env_set(&n, String::new());
    }
    finalize_input_addressed_outputs(&mut drv, cache, loader)?;
    Ok(drv)
}

/// `structuredAttrs.erase("outputChecks")` followed by nlohmann's compact `dump()`;
/// unchanged when the key is absent, so the eval-time encoding stays byte-exact.
fn erase_output_checks(json: &str) -> Result<String> {
    let mut v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| Error::Store(format!("structured attrs are not valid JSON: {e}")))?;
    let removed = v
        .as_object_mut()
        .is_some_and(|o| o.remove("outputChecks").is_some());
    Ok(if removed {
        v.to_string()
    } else {
        json.to_string()
    })
}

/// develop.cc:BuildEnvironment::Value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    String { exported: bool, value: String },
    Array(Vec<String>),
    Associative(BTreeMap<String, String>),
}

/// develop.cc:BuildEnvironment: what `get-env.sh` dumped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildEnvironment {
    pub vars: BTreeMap<String, Value>,
    pub bash_functions: BTreeMap<String, String>,
    /// `.attrs.json` and `.attrs.sh` contents, for `__structuredAttrs` shells.
    pub structured_attrs: Option<(String, String)>,
}

fn json_str(v: &serde_json::Value, what: &str) -> Result<String> {
    v.as_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Protocol(format!("{what} is not a string")))
}

impl BuildEnvironment {
    /// BuildEnvironment::fromJSON: variables of an unknown type are dropped.
    pub fn from_json(json: &serde_json::Value) -> Result<BuildEnvironment> {
        let mut res = BuildEnvironment::default();
        let vars = json["variables"]
            .as_object()
            .ok_or_else(|| Error::Protocol("environment has no 'variables' object".into()))?;
        for (name, info) in vars {
            let ty = json_str(&info["type"], "variable type")?;
            let value = &info["value"];
            let v = match ty.as_str() {
                "var" | "exported" => Value::String {
                    exported: ty == "exported",
                    value: json_str(value, "variable value")?,
                },
                "array" => Value::Array(
                    value
                        .as_array()
                        .ok_or_else(|| Error::Protocol("array variable is not a list".into()))?
                        .iter()
                        .map(|s| json_str(s, "array element"))
                        .collect::<Result<_>>()?,
                ),
                "associative" => Value::Associative(
                    value
                        .as_object()
                        .ok_or_else(|| {
                            Error::Protocol("associative variable is not an object".into())
                        })?
                        .iter()
                        .map(|(k, s)| Ok((k.clone(), json_str(s, "associative value")?)))
                        .collect::<Result<_>>()?,
                ),
                _ => continue,
            };
            res.vars.insert(name.clone(), v);
        }
        let funs = json["bashFunctions"]
            .as_object()
            .ok_or_else(|| Error::Protocol("environment has no 'bashFunctions' object".into()))?;
        for (name, def) in funs {
            res.bash_functions
                .insert(name.clone(), json_str(def, "function body")?);
        }
        if let Some(sa) = json.get("structuredAttrs") {
            res.structured_attrs = Some((
                json_str(&sa[".attrs.json"], ".attrs.json")?,
                json_str(&sa[".attrs.sh"], ".attrs.sh")?,
            ));
        }
        Ok(res)
    }

    pub fn parse(bytes: &[u8]) -> Result<BuildEnvironment> {
        let json: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|e| Error::Protocol(format!("environment file is not valid JSON: {e}")))?;
        BuildEnvironment::from_json(&json)
    }

    /// BuildEnvironment::toJSON. Keys sort bytewise, as nlohmann's `std::map` does.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::{Map, Value as J};
        let mut vars = Map::new();
        for (name, value) in &self.vars {
            let mut info = Map::new();
            match value {
                Value::String { exported, value } => {
                    info.insert(
                        "type".into(),
                        J::String(if *exported { "exported" } else { "var" }.into()),
                    );
                    info.insert("value".into(), J::String(value.clone()));
                }
                Value::Array(a) => {
                    info.insert("type".into(), J::String("array".into()));
                    info.insert(
                        "value".into(),
                        J::Array(a.iter().map(|s| J::String(s.clone())).collect()),
                    );
                }
                Value::Associative(m) => {
                    info.insert("type".into(), J::String("associative".into()));
                    info.insert(
                        "value".into(),
                        J::Object(
                            m.iter()
                                .map(|(k, v)| (k.clone(), J::String(v.clone())))
                                .collect(),
                        ),
                    );
                }
            }
            vars.insert(name.clone(), J::Object(info));
        }
        let mut res = Map::new();
        res.insert("variables".into(), J::Object(vars));
        res.insert(
            "bashFunctions".into(),
            J::Object(
                self.bash_functions
                    .iter()
                    .map(|(k, v)| (k.clone(), J::String(v.clone())))
                    .collect(),
            ),
        );
        if let Some((json, sh)) = &self.structured_attrs {
            let mut c = Map::new();
            c.insert(".attrs.sh".into(), J::String(sh.clone()));
            c.insert(".attrs.json".into(), J::String(json.clone()));
            res.insert("structuredAttrs".into(), J::Object(c));
        }
        J::Object(res)
    }

    /// BuildEnvironment::toBash.
    pub fn to_bash(&self, out: &mut String, ignore: &[&str]) {
        for (name, value) in &self.vars {
            if ignore.contains(&name.as_str()) {
                continue;
            }
            match value {
                Value::String { exported, value } => {
                    out.push_str(name);
                    out.push('=');
                    out.push_str(&escape_shell_arg_always(value));
                    out.push('\n');
                    if *exported {
                        out.push_str("export ");
                        out.push_str(name);
                        out.push('\n');
                    }
                }
                Value::Array(a) => {
                    out.push_str("declare -a ");
                    out.push_str(name);
                    out.push_str("=(");
                    for s in a {
                        out.push_str(&escape_shell_arg_always(s));
                        out.push(' ');
                    }
                    out.push_str(")\n");
                }
                Value::Associative(m) => {
                    out.push_str("declare -A ");
                    out.push_str(name);
                    out.push_str("=(");
                    for (n, v) in m {
                        out.push('[');
                        out.push_str(&escape_shell_arg_always(n));
                        out.push_str("]=");
                        out.push_str(&escape_shell_arg_always(v));
                        out.push(' ');
                    }
                    out.push_str(")\n");
                }
            }
        }
        for (name, def) in &self.bash_functions {
            out.push_str(name);
            out.push_str(" ()\n{\n");
            out.push_str(def);
            out.push_str("}\n");
        }
    }

    fn get_string(&self, name: &str) -> Result<&str> {
        match self.vars.get(name) {
            Some(Value::String { value, .. }) => Ok(value),
            Some(_) => Err(Error::Protocol(format!(
                "bash variable '{name}' is not a string"
            ))),
            None => Err(Error::Protocol(format!(
                "environment has no variable '{name}'"
            ))),
        }
    }
}

/// util.cc:escapeShellArgAlways.
pub fn escape_shell_arg_always(s: &str) -> String {
    let mut r = String::with_capacity(s.len() + 2);
    r.push('\'');
    for c in s.chars() {
        if c == '\'' {
            r.push_str("'\\''");
        } else {
            r.push(c);
        }
    }
    r.push('\'');
    r
}

/// util.cc:rewriteStrings: every rewrite, in map order, applied to every occurrence.
pub fn rewrite_strings(mut s: String, rewrites: &BTreeMap<String, String>) -> String {
    for (from, to) in rewrites {
        if from == to || from.is_empty() {
            continue;
        }
        let mut j = 0;
        while let Some(k) = s[j..].find(from.as_str()) {
            let at = j + k;
            s.replace_range(at..at + from.len(), to);
            j = at + to.len();
        }
    }
    s
}

/// The files `makeRcScript` writes for a `__structuredAttrs` shell, so a hit can check
/// they still exist.
pub struct RcScript {
    pub script: String,
    pub files: Vec<PathBuf>,
}

/// develop.cc:Common::makeRcScript. `outputs_dir` is `<cwd>/outputs` in Nix; `attrs_dir`
/// receives `.attrs.sh` and `.attrs.json` for a structured-attrs shell (Nix writes them to a
/// temporary directory it deletes on exit; here they must outlive the process).
pub fn make_rc_script(
    env: &BuildEnvironment,
    attrs_dir: &Path,
    outputs_dir: &Path,
) -> Result<RcScript> {
    let mut out = String::with_capacity(64 << 10);
    out.push_str("unset shellHook\n");
    for var in SAVED_VARS {
        out.push_str(&format!("{var}=${{{var}:-}}\nnix_saved_{var}=\"${var}\"\n"));
    }
    env.to_bash(&mut out, IGNORE_VARS);
    for var in SAVED_VARS {
        out.push_str(&format!(
            "{var}=\"${var}${{nix_saved_{var}:+:$nix_saved_{var}}}\"\n"
        ));
    }
    out.push_str("export NIX_BUILD_TOP=\"$(mktemp -d -t nix-shell.XXXXXX)\"\n");
    for var in ["TMP", "TMPDIR", "TEMP", "TEMPDIR"] {
        out.push_str(&format!("export {var}=\"$NIX_BUILD_TOP\"\n"));
    }
    out.push_str("eval \"${shellHook:-}\"\n");

    let mut rewrites = BTreeMap::new();
    let outputs = env
        .vars
        .get("outputs")
        .ok_or_else(|| Error::Protocol("environment has no 'outputs' variable".into()))?;
    match outputs {
        Value::Associative(m) => {
            for (name, from) in m {
                rewrites.insert(
                    from.clone(),
                    outputs_dir.join(name).to_string_lossy().into_owned(),
                );
            }
        }
        Value::String { value, .. } => {
            for name in value
                .split([' ', '\t', '\n', '\r'])
                .filter(|t| !t.is_empty())
            {
                rewrites.insert(
                    env.get_string(name)?.to_string(),
                    outputs_dir.join(name).to_string_lossy().into_owned(),
                );
            }
        }
        Value::Array(a) => {
            for name in a {
                rewrites.insert(
                    env.get_string(name)?.to_string(),
                    outputs_dir.join(name).to_string_lossy().into_owned(),
                );
            }
        }
    }

    let mut files = Vec::new();
    if let Some((json, sh)) = &env.structured_attrs {
        std::fs::create_dir_all(attrs_dir)
            .map_err(|e| Error::io(format!("cannot create '{}'", attrs_dir.display()), e))?;
        for (ext, var, content) in [
            ("sh", "NIX_ATTRS_SH_FILE", sh),
            ("json", "NIX_ATTRS_JSON_FILE", json),
        ] {
            let target = attrs_dir.join(format!(".attrs.{ext}"));
            write_file(&target, content.as_bytes())
                .map_err(|e| Error::io(format!("cannot write '{}'", target.display()), e))?;
            rewrites.insert(
                env.get_string(var)?.to_string(),
                target.to_string_lossy().into_owned(),
            );
            files.push(target);
        }
    }
    Ok(RcScript {
        script: rewrite_strings(out, &rewrites),
        files,
    })
}

fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// What a `print-dev-env` cache entry stores: the env output, the files the printed bytes
/// refer to, and the bytes themselves.
pub struct CachedResult {
    pub out_path: String,
    pub files: Vec<PathBuf>,
    pub bytes: Vec<u8>,
}

impl CachedResult {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.bytes.len() + 256);
        v.extend_from_slice(self.out_path.as_bytes());
        v.push(b'\n');
        for f in &self.files {
            v.extend_from_slice(f.as_os_str().as_encoded_bytes());
            v.push(b'\n');
        }
        v.push(b'\n');
        v.extend_from_slice(&self.bytes);
        v
    }

    pub fn decode(b: &[u8]) -> Option<CachedResult> {
        let mut lines = Vec::new();
        let mut rest = b;
        loop {
            let nl = rest.iter().position(|&c| c == b'\n')?;
            let line = &rest[..nl];
            rest = &rest[nl + 1..];
            if line.is_empty() {
                break;
            }
            lines.push(line);
        }
        let (out_path, files) = lines.split_first()?;
        Some(CachedResult {
            out_path: String::from_utf8(out_path.to_vec()).ok()?,
            files: files
                .iter()
                .map(|f| PathBuf::from(String::from_utf8_lossy(f).into_owned()))
                .collect(),
            bytes: rest.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::drv::load_derivation_from_store;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/drv")
                .join(name),
        )
        .unwrap()
    }

    /// Nix 2.34.8 added this file with `nix print-dev-env`; the path is in every `-env.drv`
    /// it wrote (`grep get-env.sh /nix/store/*-env.drv`).
    #[test]
    fn get_env_sh_store_path_matches_nix() {
        assert_eq!(
            get_env_sh_path().to_string(),
            "/nix/store/hyhh9rhzxhzmfspllmpf75xzkzns109r-get-env.sh"
        );
    }

    /// Both fixtures are this repo's devShell: `nix eval --raw .#devShells.x86_64-linux.default.drvPath`
    /// names the shell drv and `nix print-dev-env .` writes the `-env.drv` (2026-09-17); the
    /// transformation must reproduce the `-env.drv` byte for byte. Needs the shell's input
    /// closure in /nix/store for the hash modulo: 1815 .drv files, too many to vendor.
    #[test]
    #[ignore = "needs the devShell input .drv closure in /nix/store"]
    fn env_derivation_matches_nix() {
        let shell_path = "1rpf3nddnjznxv3q2bf64vpn8zdz1ciw-nix-shell.drv";
        let env_path = "vlcxckkq3qad5dp748a5xypnkwq76azw-nix-shell-env.drv";
        let shell = Derivation::parse(&fixture(shell_path), "nix-shell").unwrap();
        let mut cache = DrvHashCache::new();
        let mut loader = load_derivation_from_store;
        let env = env_derivation(&shell, &mut cache, &mut loader).unwrap();
        assert_eq!(env.name, "nix-shell-env");
        assert_eq!(env.unparse().unwrap(), fixture(env_path));
        assert_eq!(
            env.drv_path().unwrap().to_string(),
            format!("/nix/store/{env_path}")
        );
    }

    #[test]
    fn non_bash_builder_is_refused() {
        let drv = Derivation {
            builder: "/bin/sh".into(),
            ..Default::default()
        };
        let mut cache = DrvHashCache::new();
        assert!(env_derivation(&drv, &mut cache, &mut load_derivation_from_store).is_err());
    }

    #[test]
    fn output_checks_are_erased_only_when_present() {
        assert_eq!(
            erase_output_checks(r#"{"b":1,"outputChecks":{"out":{}},"a":"x"}"#).unwrap(),
            r#"{"a":"x","b":1}"#
        );
        let same = r#"{"z":1,"a":[1,2]}"#;
        assert_eq!(erase_output_checks(same).unwrap(), same);
    }

    #[test]
    fn shell_escaping() {
        assert_eq!(escape_shell_arg_always(""), "''");
        assert_eq!(escape_shell_arg_always("a b"), "'a b'");
        assert_eq!(escape_shell_arg_always("it's"), "'it'\\''s'");
    }

    #[test]
    fn rewrites_apply_in_map_order_to_every_occurrence() {
        let mut r = BTreeMap::new();
        r.insert("/nix/store/x-out".to_string(), "/w/outputs/out".to_string());
        r.insert("same".to_string(), "same".to_string());
        assert_eq!(
            rewrite_strings("a=/nix/store/x-out:/nix/store/x-out same".into(), &r),
            "a=/w/outputs/out:/w/outputs/out same"
        );
    }

    #[test]
    fn json_roundtrip_and_bash_forms() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
              "bashFunctions": {"f": " \n    echo hi\n"},
              "variables": {
                "A": {"type": "exported", "value": "it's"},
                "B": {"type": "var", "value": "b"},
                "L": {"type": "array", "value": ["x", "y z"]},
                "M": {"type": "associative", "value": {"k": "v", "a": "b"}},
                "TERM": {"type": "exported", "value": "dumb"},
                "U": {"type": "unknown"}
              }
            }"#,
        )
        .unwrap();
        let env = BuildEnvironment::from_json(&json).unwrap();
        assert!(!env.vars.contains_key("U"));
        assert_eq!(BuildEnvironment::from_json(&env.to_json()).unwrap(), env);
        let mut s = String::new();
        env.to_bash(&mut s, IGNORE_VARS);
        assert_eq!(
            s,
            "A='it'\\''s'\nexport A\nB='b'\ndeclare -a L=('x' 'y z' )\ndeclare -A M=(['a']='b' ['k']='v' )\nf ()\n{\n \n    echo hi\n}\n"
        );
    }

    #[test]
    fn rc_script_rewrites_outputs_and_writes_attrs() {
        let dir = crate::store::nar::tests::fixture_tree("devenv-rc");
        let json: serde_json::Value = serde_json::from_str(
            r#"{
              "bashFunctions": {},
              "variables": {
                "outputs": {"type": "var", "value": "out dev"},
                "out": {"type": "exported", "value": "/nix/store/a-x-env"},
                "dev": {"type": "exported", "value": "/nix/store/b-x-env-dev"},
                "NIX_LDFLAGS": {"type": "exported", "value": "-rpath /nix/store/a-x-env/lib"},
                "NIX_ATTRS_SH_FILE": {"type": "exported", "value": "/build/.attrs.sh"},
                "NIX_ATTRS_JSON_FILE": {"type": "exported", "value": "/build/.attrs.json"}
              },
              "structuredAttrs": {".attrs.sh": "declare x=1\n", ".attrs.json": "{}"}
            }"#,
        )
        .unwrap();
        let env = BuildEnvironment::from_json(&json).unwrap();
        let rc = make_rc_script(&env, &dir.join("attrs"), Path::new("/w/outputs")).unwrap();
        assert!(rc.script.starts_with(
            "unset shellHook\nPATH=${PATH:-}\nnix_saved_PATH=\"$PATH\"\nXDG_DATA_DIRS=${XDG_DATA_DIRS:-}\nnix_saved_XDG_DATA_DIRS=\"$XDG_DATA_DIRS\"\n"
        ));
        assert!(rc.script.ends_with(
            "PATH=\"$PATH${nix_saved_PATH:+:$nix_saved_PATH}\"\nXDG_DATA_DIRS=\"$XDG_DATA_DIRS${nix_saved_XDG_DATA_DIRS:+:$nix_saved_XDG_DATA_DIRS}\"\nexport NIX_BUILD_TOP=\"$(mktemp -d -t nix-shell.XXXXXX)\"\nexport TMP=\"$NIX_BUILD_TOP\"\nexport TMPDIR=\"$NIX_BUILD_TOP\"\nexport TEMP=\"$NIX_BUILD_TOP\"\nexport TEMPDIR=\"$NIX_BUILD_TOP\"\neval \"${shellHook:-}\"\n"
        ));
        assert!(
            rc.script
                .contains("NIX_LDFLAGS='-rpath /w/outputs/out/lib'\n")
        );
        assert!(rc.script.contains("dev='/w/outputs/dev'\n"));
        let sh = dir.join("attrs/.attrs.sh");
        assert_eq!(std::fs::read_to_string(&sh).unwrap(), "declare x=1\n");
        assert!(
            rc.script
                .contains(&format!("NIX_ATTRS_SH_FILE='{}'\n", sh.display()))
        );
        assert_eq!(rc.files.len(), 2);
    }

    #[test]
    fn cached_result_roundtrip() {
        let r = CachedResult {
            out_path: "/nix/store/x-env".into(),
            files: vec![PathBuf::from("/a/.attrs.sh")],
            bytes: b"x=1\n\nexport x\n".to_vec(),
        };
        let d = CachedResult::decode(&r.encode()).unwrap();
        assert_eq!(d.out_path, r.out_path);
        assert_eq!(d.files, r.files);
        assert_eq!(d.bytes, r.bytes);
        let r = CachedResult { files: vec![], ..r };
        assert!(CachedResult::decode(&r.encode()).unwrap().files.is_empty());
        assert!(CachedResult::decode(b"").is_none());
    }
}
