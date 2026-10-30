//! The worker-protocol serialisations the gRPC messages carry as opaque bytes:
//! `BasicDerivation` in a `BuildDerivationRequest`, `BuildResult` and
//! `vector<KeyedBuildResult>` in the replies, and `UnkeyedValidPathInfo` at
//! protocol 1.16 in a `PathInfo`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};

use crate::error::{Error, Result};
use crate::store::drv::{Derivation, DerivationOutput, STRUCTURED_ATTRS_ENV};
use crate::store::path::StorePath;

use super::super::worker::wire::{
    self, read_bool, read_opt_path, read_paths, read_str, read_strs, read_time, read_u64,
    write_str, write_strs, write_u64,
};

/// nix-compat.hh:`kBuildProtocolWire` — `major << 8 | minor`. The daemon
/// rejects anything else with `INVALID_ARGUMENT`.
pub const BUILD_PROTOCOL: u32 = (1 << 8) | 37;

/// derivation/aterm.cc:`write(Sink &, const StoreDirConfig &, const Basic &)`.
/// `name` is not on the wire: the reader takes it from the drv path.
/// `input_srcs` comes from `basic_input_srcs`.
pub fn write_basic_derivation(
    w: &mut dyn Write,
    drv: &Derivation,
    input_srcs: &BTreeSet<StorePath>,
) -> io::Result<()> {
    write_u64(w, drv.outputs.len() as u64)?;
    for (name, out) in &drv.outputs {
        write_str(w, name.as_bytes())?;
        match out {
            DerivationOutput::InputAddressed(p) => {
                write_str(w, p.to_string().as_bytes())?;
                write_str(w, b"")?;
                write_str(w, b"")?;
            }
            DerivationOutput::Fixed { method, hash } => {
                let path = out
                    .fixed_path(&drv.name, name)
                    .ok()
                    .flatten()
                    .ok_or_else(|| {
                        io::Error::other(format!("output '{name}' has no fixed path"))
                    })?;
                write_str(w, path.to_string().as_bytes())?;
                write_str(
                    w,
                    format!("{}{}", method.prefix(), hash.algo.name()).as_bytes(),
                )?;
                write_str(w, hash.to_base16().as_bytes())?;
            }
            DerivationOutput::CaFloating { method, algo } => {
                write_str(w, b"")?;
                write_str(w, format!("{}{}", method.prefix(), algo.name()).as_bytes())?;
                write_str(w, b"")?;
            }
            DerivationOutput::Deferred => {
                write_str(w, b"")?;
                write_str(w, b"")?;
                write_str(w, b"")?;
            }
        }
    }
    let srcs: Vec<String> = input_srcs.iter().map(StorePath::to_string).collect();
    write_strs(w, &srcs)?;
    write_str(w, drv.platform.as_bytes())?;
    write_str(w, drv.builder.as_bytes())?;
    write_strs(w, &drv.args)?;
    // StructuredAttrs::unparse puts the JSON back under `__json`; the env map
    // is sorted by key, so the entry goes where its key belongs.
    let mut env: BTreeMap<&str, &[u8]> = drv.env.iter().map(|(k, v)| (&k[..], &v[..])).collect();
    if let Some(json) = &drv.structured_attrs {
        env.insert(STRUCTURED_ATTRS_ENV, json.as_bytes());
    }
    write_u64(w, env.len() as u64)?;
    for (k, v) in env {
        write_str(w, k.as_bytes())?;
        write_str(w, v)?;
    }
    Ok(())
}

/// worker-protocol.cc:`Serialise<BuildResult>`, at protocol 1.37 with no
/// negotiated features (so `builtOutputs` is the legacy `StringMap` keyed
/// `<dummy hash>!<output name>` with JSON values).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildResult {
    pub status: u64,
    pub error_msg: String,
    pub times_built: u64,
    pub is_non_deterministic: bool,
    pub start_time: u64,
    pub stop_time: u64,
    pub cpu_user_us: Option<u64>,
    pub cpu_system_us: Option<u64>,
    /// Output name to store path, from `builtOutputs`.
    pub built_outputs: BTreeMap<String, String>,
}

impl BuildResult {
    /// common-protocol.cc:`buildResultStatusTable`: Built, Substituted,
    /// AlreadyValid, ResolvesToAlreadyValid.
    pub fn success(&self) -> bool {
        matches!(self.status, 0 | 1 | 2 | 13)
    }

    /// `TransientFailure` (6) is what the daemon turns into `UNAVAILABLE` for a
    /// retry; the others are the build's own verdict.
    pub fn read(r: &mut dyn io::Read) -> io::Result<BuildResult> {
        let mut res = BuildResult {
            status: read_u64(r)?,
            error_msg: read_str(r)?,
            ..BuildResult::default()
        };
        res.times_built = read_u64(r)?;
        res.is_non_deterministic = read_u64(r)? != 0;
        res.start_time = read_u64(r)?;
        res.stop_time = read_u64(r)?;
        res.cpu_user_us = read_opt_u64(r)?;
        res.cpu_system_us = read_opt_u64(r)?;
        let n = read_u64(r)?;
        for _ in 0..n {
            let key = read_str(r)?;
            let json = read_str(r)?;
            let name = key
                .split_once('!')
                .map_or(key.clone(), |(_, n)| n.to_string());
            let path = serde_json::from_str::<serde_json::Value>(&json)
                .ok()
                .and_then(|v| {
                    v.get("outPath")
                        .and_then(|p| p.as_str().map(str::to_string))
                });
            if let Some(path) = path {
                res.built_outputs.insert(name, path);
            }
        }
        Ok(res)
    }
}

fn read_opt_u64(r: &mut dyn io::Read) -> io::Result<Option<u64>> {
    if read_u64(r)? == 1 {
        Ok(Some(read_u64(r)?))
    } else {
        Ok(None)
    }
}

/// One path's `UnkeyedValidPathInfo` at protocol 1.16, as it arrives in a
/// `PathInfo` message (path-info-wire.hh:`encodePathInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathInfo {
    pub path: StorePath,
    pub deriver: String,
    /// Base16, as the wire carries it.
    pub nar_hash: String,
    pub references: Vec<String>,
    pub registration_time: u64,
    pub nar_size: u64,
    pub ultimate: bool,
    pub sigs: Vec<String>,
    pub ca: String,
}

impl PathInfo {
    /// `entry.path` is a store path base name; `entry.info` the 1.16 bytes.
    pub fn decode(path: &str, info: &[u8]) -> Result<PathInfo> {
        let mut r = info;
        PathInfo::fields(StorePath::from_base_name(path)?, &mut r)
    }

    /// The same fields straight off a worker-protocol connection, where `path`
    /// is the full store path the caller asked `QueryPathInfo` about.
    pub fn read(path: &str, r: &mut dyn Read) -> Result<PathInfo> {
        PathInfo::fields(StorePath::parse(path)?, r)
    }

    fn fields(path: StorePath, r: &mut dyn Read) -> Result<PathInfo> {
        let deriver = read_opt_path(r)?;
        let nar_hash = wire::nar_hash(&read_str(r)?)?;
        let references = read_paths(r)?;
        let registration_time = read_time(r)?;
        let nar_size = read_u64(r)?;
        let ultimate = read_bool(r)?;
        let sigs = read_strs(r)?
            .iter()
            .map(|s| wire::signature(s))
            .collect::<io::Result<_>>()?;
        let ca = wire::content_address(&read_str(r)?)?;
        Ok(PathInfo {
            path,
            deriver,
            nar_hash,
            references,
            registration_time,
            nar_size,
            ultimate,
            sigs,
            ca,
        })
    }

    /// The `ValidPathInfo` an `AddMultipleToStore` body wants for this path
    /// (worker-protocol.cc:`Serialise<ValidPathInfo>` at 1.16): the path, then
    /// the same fields back, signatures included, so a local daemon can verify
    /// what the farm signed.
    pub fn write(&self, w: &mut dyn Write) -> io::Result<()> {
        write_str(w, self.path.to_string().as_bytes())?;
        write_str(w, self.deriver.as_bytes())?;
        write_str(w, self.nar_hash.as_bytes())?;
        write_strs(w, &self.references)?;
        write_u64(w, self.registration_time)?;
        write_u64(w, self.nar_size)?;
        write_u64(w, u64::from(self.ultimate))?;
        write_strs(w, &self.sigs)?;
        write_str(w, self.ca.as_bytes())
    }
}

/// Turns a `Derivation` into the `input_srcs` a `BasicDerivation` carries: its
/// own `inputSrcs` plus the output path of every input derivation output
/// (nix-compat.hh:`toBasicDrv`). `output_of` looks the latter up.
pub fn basic_input_srcs(
    drv_path: &StorePath,
    drv: &Derivation,
    output_of: &dyn Fn(&StorePath, &str) -> Option<StorePath>,
) -> Result<BTreeSet<StorePath>> {
    let mut srcs = drv.input_srcs.clone();
    for (dep, node) in &drv.input_drvs {
        for o in &node.outputs {
            let Some(p) = output_of(dep, o) else {
                return Err(Error::Plan(format!(
                    "'{drv_path}': input '{dep}!{o}' has no statically known path; \
                     the build farm needs statically known input and output paths"
                )));
            };
            srcs.insert(p);
        }
    }
    Ok(srcs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::drv::Derivation;

    const ATERM: &str = r#"Derive([("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo","","")],[("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dep.drv",["out"])],["/nix/store/cccccccccccccccccccccccccccccccc-src"],"x86_64-linux","/bin/sh",["-c","true"],[("__json","{\"a\":1}"),("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo"),("zz","last")])"#;

    fn fixture() -> Derivation {
        Derivation::parse(ATERM.as_bytes(), "foo").unwrap()
    }

    /// Reproduces `derivation::write` by hand: the field order and the empty
    /// hash-algo/hash strings for an input-addressed output.
    #[test]
    fn basic_derivation_matches_the_cxx_field_order() {
        let drv = fixture();
        let dep_out =
            StorePath::parse("/nix/store/dddddddddddddddddddddddddddddddd-dep-out").unwrap();
        let srcs = basic_input_srcs(
            &StorePath::parse("/nix/store/ffffffffffffffffffffffffffffffff-foo.drv").unwrap(),
            &drv,
            &|_, _| Some(dep_out.clone()),
        )
        .unwrap();
        let mut got = Vec::new();
        write_basic_derivation(&mut got, &drv, &srcs).unwrap();

        let mut want = Vec::new();
        write_u64(&mut want, 1).unwrap();
        write_str(&mut want, b"out").unwrap();
        write_str(
            &mut want,
            b"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo",
        )
        .unwrap();
        write_str(&mut want, b"").unwrap();
        write_str(&mut want, b"").unwrap();
        // inputSrcs, sorted by base name: the src then the resolved dep output.
        write_strs(
            &mut want,
            &[
                "/nix/store/cccccccccccccccccccccccccccccccc-src",
                "/nix/store/dddddddddddddddddddddddddddddddd-dep-out",
            ],
        )
        .unwrap();
        write_str(&mut want, b"x86_64-linux").unwrap();
        write_str(&mut want, b"/bin/sh").unwrap();
        write_strs(&mut want, &["-c", "true"]).unwrap();
        write_u64(&mut want, 3).unwrap();
        for (k, v) in [
            ("__json", "{\"a\":1}"),
            ("out", "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo"),
            ("zz", "last"),
        ] {
            write_str(&mut want, k.as_bytes()).unwrap();
            write_str(&mut want, v.as_bytes()).unwrap();
        }
        assert_eq!(got, want);
    }

    #[test]
    fn an_unresolvable_input_output_is_an_error() {
        let drv = fixture();
        let path = StorePath::parse("/nix/store/ffffffffffffffffffffffffffffffff-foo.drv").unwrap();
        assert!(basic_input_srcs(&path, &drv, &|_, _| None).is_err());
    }

    #[test]
    fn build_result_round_trips_a_daemon_reply() {
        let mut bytes = Vec::new();
        write_u64(&mut bytes, 0).unwrap(); // Built
        write_str(&mut bytes, b"").unwrap();
        write_u64(&mut bytes, 1).unwrap(); // timesBuilt
        write_u64(&mut bytes, 0).unwrap(); // isNonDeterministic
        write_u64(&mut bytes, 1000).unwrap();
        write_u64(&mut bytes, 1042).unwrap();
        write_u64(&mut bytes, 1).unwrap(); // cpuUser present
        write_u64(&mut bytes, 12345).unwrap();
        write_u64(&mut bytes, 0).unwrap(); // cpuSystem absent
        write_u64(&mut bytes, 1).unwrap(); // one built output
        write_str(&mut bytes, b"sha256:0000!out").unwrap();
        write_str(
            &mut bytes,
            br#"{"id":"sha256:0000!out","outPath":"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo"}"#,
        )
        .unwrap();

        let res = BuildResult::read(&mut bytes.as_slice()).unwrap();
        assert!(res.success());
        assert_eq!(res.times_built, 1);
        assert_eq!(res.stop_time, 1042);
        assert_eq!(res.cpu_user_us, Some(12345));
        assert_eq!(res.cpu_system_us, None);
        assert_eq!(
            res.built_outputs.get("out").map(String::as_str),
            Some("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo")
        );
    }

    #[test]
    fn a_failure_keeps_its_message() {
        let mut bytes = Vec::new();
        write_u64(&mut bytes, 3).unwrap(); // PermanentFailure
        write_str(&mut bytes, b"builder failed").unwrap();
        for _ in 0..4 {
            write_u64(&mut bytes, 0).unwrap();
        }
        write_u64(&mut bytes, 0).unwrap();
        write_u64(&mut bytes, 0).unwrap();
        write_u64(&mut bytes, 0).unwrap();
        let res = BuildResult::read(&mut bytes.as_slice()).unwrap();
        assert!(!res.success());
        assert_eq!(res.error_msg, "builder failed");
    }

    #[test]
    fn path_info_round_trips() {
        let info = PathInfo {
            path: StorePath::from_base_name("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo").unwrap(),
            deriver: "/nix/store/ffffffffffffffffffffffffffffffff-foo.drv".into(),
            nar_hash: "0".repeat(64),
            references: vec!["/nix/store/cccccccccccccccccccccccccccccccc-src".into()],
            registration_time: 7,
            nar_size: 4096,
            ultimate: false,
            sigs: vec!["cache.numtide.com-1:abc=".into()],
            ca: String::new(),
        };
        // `write` prefixes the path, `decode` takes it out of band, so drop it.
        let mut bytes = Vec::new();
        info.write(&mut bytes).unwrap();
        let mut r = bytes.as_slice();
        assert_eq!(read_str(&mut r).unwrap(), info.path.to_string());
        let decoded = PathInfo::decode(info.path.base_name(), r).unwrap();
        assert_eq!(decoded, info);
    }
}
