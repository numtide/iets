//! `nix derivation show`'s JSON: the derivations a set of roots names, and
//! with `recursive` their whole input-derivation closure. Each ATerm comes
//! from this evaluation's derivation table, so the answer needs nothing in
//! `/nix/store`; a drv the evaluation did not make is read from there
//! (docs/reference/classic.md, docs/reference/divergences.md).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::iter;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::builtins::derivation::DrvInfo;
use crate::error::{Error, Result};
use crate::eval::Evaluator;
use crate::print;
use crate::remote::plan::{load_drv, walk};
use crate::store::drv::{Derivation, DerivationOutput};
use crate::store::path::{IngestionMethod, StorePath};

/// derivations.hh:expectedJsonVersionDerivation.
const VERSION: u64 = 4;

/// The whole `nix derivation show` document: `{"derivations": {…}, "version":
/// 4}`, keyed by base name as `StorePath::to_string` renders it.
pub fn show(ev: &Evaluator, roots: &[StorePath], recursive: bool) -> Result<Value> {
    let table = ev.drv.drvs.lock();
    let nodes = if recursive {
        closure(roots, &table)?
    } else {
        tops(roots, &table)?
    };
    let mut drvs = Map::new();
    for (path, aterm) in nodes {
        let drv = Derivation::parse(&aterm, Derivation::name_from_path(&path)?)?;
        drvs.insert(path.base_name().to_string(), derivation(&drv)?);
    }
    let mut out = Map::new();
    out.insert("derivations".into(), Value::Object(drvs));
    out.insert("version".into(), Value::from(VERSION));
    Ok(Value::Object(out))
}

/// `nlohmann::json::dump`, which `serde_json` matches for everything but a
/// float: `structuredAttrs` can hold one, and nlohmann writes the shortest
/// round-trip form with a two-digit exponent (`print::json_float`).
pub fn json_text(v: &Value, pretty: bool) -> Vec<u8> {
    let mut out = Vec::new();
    write_json(&mut out, v, pretty.then_some(0));
    out.push(b'\n');
    out
}

fn write_json(out: &mut Vec<u8>, v: &Value, indent: Option<usize>) {
    fn newline(out: &mut Vec<u8>, depth: usize) {
        out.push(b'\n');
        out.extend(iter::repeat_n(b' ', depth * 2));
    }
    let inner = indent.map(|d| d + 1);
    match v {
        Value::Number(n) if n.as_i64().is_none() && n.as_u64().is_none() => {
            let f = n.as_f64().unwrap_or(f64::NAN);
            out.extend_from_slice(print::json_float(f).as_bytes());
        }
        Value::Array(a) if !a.is_empty() => {
            out.push(b'[');
            for (i, e) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                if let Some(d) = inner {
                    newline(out, d);
                }
                write_json(out, e, inner);
            }
            if let Some(d) = indent {
                newline(out, d);
            }
            out.push(b']');
        }
        Value::Object(o) if !o.is_empty() => {
            out.push(b'{');
            for (i, (k, e)) in o.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                if let Some(d) = inner {
                    newline(out, d);
                }
                out.extend_from_slice(&serde_json::to_vec(k).unwrap_or_default());
                out.push(b':');
                if indent.is_some() {
                    out.push(b' ');
                }
                write_json(out, e, inner);
            }
            if let Some(d) = indent {
                newline(out, d);
            }
            out.push(b'}');
        }
        _ => out.extend_from_slice(&serde_json::to_vec(v).unwrap_or_default()),
    }
}

/// The roots themselves, deduplicated. A root that is not a `.drv` is
/// dropped, as derivation-show.cc drops it.
fn tops(
    roots: &[StorePath],
    table: &HashMap<StorePath, DrvInfo>,
) -> Result<Vec<(StorePath, Arc<Vec<u8>>)>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for p in roots {
        if !p.is_derivation() || !seen.insert(p.clone()) {
            continue;
        }
        let (aterm, _, _) = load_drv(p, &|q| table.get(q).cloned())?;
        out.push((p.clone(), aterm));
    }
    Ok(out)
}

/// `-r`: every derivation the roots reach through `inputDrvs`. This is
/// `computeFSClosure` over the roots with the sources dropped, except that the
/// closure is read from the derivation table first and `/nix/store` only for
/// what the table lacks.
fn closure(
    roots: &[StorePath],
    table: &HashMap<StorePath, DrvInfo>,
) -> Result<Vec<(StorePath, Arc<Vec<u8>>)>> {
    let roots: Vec<StorePath> = roots
        .iter()
        .filter(|p| p.is_derivation())
        .cloned()
        .collect();
    let (nodes, _, _) = walk(&roots, &HashSet::new(), &|q| table.get(q).cloned(), &|_| {
        None
    })?;
    Ok(nodes.into_iter().map(|(p, aterm, _)| (p, aterm)).collect())
}

/// derivations.cc:adl_serializer<Derivation>::to_json. The keys are sorted,
/// because nlohmann's object is a sorted map and so is `serde_json`'s.
fn derivation(drv: &Derivation) -> Result<Value> {
    let mut o = Map::new();
    o.insert("args".into(), strings(&drv.args));
    o.insert("builder".into(), Value::from(drv.builder.as_str()));
    o.insert(
        "env".into(),
        Value::Object(
            (drv.env.iter())
                .map(|(k, v)| (k.clone(), Value::from(String::from_utf8_lossy(v))))
                .collect(),
        ),
    );
    o.insert("inputs".into(), inputs(drv));
    o.insert("name".into(), Value::from(drv.name.as_str()));
    o.insert(
        "outputs".into(),
        Value::Object(
            (drv.outputs.iter())
                .map(|(name, out)| (name.clone(), output(out)))
                .collect(),
        ),
    );
    if let Some(json) = &drv.structured_attrs {
        // nix parses `__json` out of the env and dumps the parsed value, so a
        // non-canonical text is renormalised rather than passed through.
        let parsed = serde_json::from_str(json).map_err(|e| {
            Error::Store(format!(
                "derivation '{}' has unparsable structured attrs: {e}",
                drv.name
            ))
        })?;
        o.insert("structuredAttrs".into(), parsed);
    }
    o.insert("system".into(), Value::from(drv.platform.as_str()));
    o.insert("version".into(), Value::from(VERSION));
    Ok(Value::Object(o))
}

fn inputs(drv: &Derivation) -> Value {
    let mut o = Map::new();
    o.insert(
        "drvs".into(),
        Value::Object(
            (drv.input_drvs.iter())
                .map(|(p, node)| (p.base_name().to_string(), input_drv(&node.outputs)))
                .collect(),
        ),
    );
    o.insert("srcs".into(), paths(&drv.input_srcs));
    Value::Object(o)
}

/// One `inputDrvs` entry. `dynamicOutputs` is always empty: iets refuses a
/// dynamic-derivation input when it parses the ATerm.
fn input_drv(outputs: &BTreeSet<String>) -> Value {
    let mut o = Map::new();
    o.insert("dynamicOutputs".into(), Value::Object(Map::new()));
    o.insert(
        "outputs".into(),
        Value::Array(outputs.iter().map(|s| Value::from(s.as_str())).collect()),
    );
    Value::Object(o)
}

/// derivations.cc:adl_serializer<DerivationOutput>::to_json. A fixed output
/// carries its content address, not its path, which nix leaves out because the
/// serializer does not know the store directory.
fn output(out: &DerivationOutput) -> Value {
    let mut o = Map::new();
    match out {
        DerivationOutput::InputAddressed(p) => {
            o.insert("path".into(), Value::from(p.base_name()));
        }
        DerivationOutput::Fixed { method, hash } => {
            o.insert("hash".into(), Value::from(hash.to_sri()));
            o.insert("method".into(), Value::from(render(*method)));
        }
        DerivationOutput::CaFloating { method, algo } => {
            o.insert("hashAlgo".into(), Value::from(algo.name()));
            o.insert("method".into(), Value::from(render(*method)));
        }
        DerivationOutput::Deferred => {}
    }
    Value::Object(o)
}

/// content-address.cc:ContentAddressMethod::render.
fn render(m: IngestionMethod) -> &'static str {
    match m {
        IngestionMethod::Flat => "flat",
        IngestionMethod::Recursive => "nar",
        IngestionMethod::Text => "text",
    }
}

fn strings(v: &[Vec<u8>]) -> Value {
    Value::Array(
        v.iter()
            .map(|s| Value::from(String::from_utf8_lossy(s)))
            .collect(),
    )
}

fn paths(v: &BTreeSet<StorePath>) -> Value {
    Value::Array(v.iter().map(|p| Value::from(p.base_name())).collect())
}

/// Each pair is one `.drv` and what `nix derivation show` printed for it,
/// captured from `nix (Nix) 2.34.8` on a case built for it.
#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn json_of(aterm: &str, name: &str) -> String {
        let drv = Derivation::parse(aterm.as_bytes(), name).unwrap();
        derivation(&drv).unwrap().to_string()
    }

    #[rstest]
    #[case::a_multi_output_derivation(
        r#"Derive([("dev","/nix/store/y7a2ldlsm4a9b89dy8gjq9w2jxrg9ni7-drvshow-multi-dev","",""),("out","/nix/store/glmm5g4k4ni9sqr84qzq47c7z839qy2v-drvshow-multi","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo a > $out; echo b > $dev"],[("builder","/bin/sh"),("dev","/nix/store/y7a2ldlsm4a9b89dy8gjq9w2jxrg9ni7-drvshow-multi-dev"),("name","drvshow-multi"),("out","/nix/store/glmm5g4k4ni9sqr84qzq47c7z839qy2v-drvshow-multi"),("outputs","out dev"),("system","x86_64-linux")])"#,
        "drvshow-multi",
        r#"{"args":["-c","echo a > $out; echo b > $dev"],"builder":"/bin/sh","env":{"builder":"/bin/sh","dev":"/nix/store/y7a2ldlsm4a9b89dy8gjq9w2jxrg9ni7-drvshow-multi-dev","name":"drvshow-multi","out":"/nix/store/glmm5g4k4ni9sqr84qzq47c7z839qy2v-drvshow-multi","outputs":"out dev","system":"x86_64-linux"},"inputs":{"drvs":{},"srcs":[]},"name":"drvshow-multi","outputs":{"dev":{"path":"y7a2ldlsm4a9b89dy8gjq9w2jxrg9ni7-drvshow-multi-dev"},"out":{"path":"glmm5g4k4ni9sqr84qzq47c7z839qy2v-drvshow-multi"}},"system":"x86_64-linux","version":4}"#
    )]
    #[case::a_fixed_output_shows_its_content_address_and_no_path(
        r#"Derive([("out","/nix/store/fzwqzgk7sxbw469w1hhbp686hlxgwpkx-drvshow-fod","r:sha1","0000000000000000000000000000000000000000")],[],[],"x86_64-linux","/bin/sh",["-c","echo x > $out"],[("builder","/bin/sh"),("name","drvshow-fod"),("out","/nix/store/fzwqzgk7sxbw469w1hhbp686hlxgwpkx-drvshow-fod"),("outputHash","0000000000000000000000000000000000000000"),("outputHashAlgo","sha1"),("outputHashMode","recursive"),("system","x86_64-linux")])"#,
        "drvshow-fod",
        r#"{"args":["-c","echo x > $out"],"builder":"/bin/sh","env":{"builder":"/bin/sh","name":"drvshow-fod","out":"/nix/store/fzwqzgk7sxbw469w1hhbp686hlxgwpkx-drvshow-fod","outputHash":"0000000000000000000000000000000000000000","outputHashAlgo":"sha1","outputHashMode":"recursive","system":"x86_64-linux"},"inputs":{"drvs":{},"srcs":[]},"name":"drvshow-fod","outputs":{"out":{"hash":"sha1-AAAAAAAAAAAAAAAAAAAAAAAAAAA=","method":"nar"}},"system":"x86_64-linux","version":4}"#
    )]
    #[case::inputs_and_structured_attrs(
        r#"Derive([("out","/nix/store/5railz2b3ssvqdssggdsnga8dyahvbaj-drvshow-top","","")],[("/nix/store/swp3gmqyvz1k2yjkmncfb29zhdhqfjmn-drvshow-dep.drv",["dev"])],["/nix/store/yb554rm2xvp2vbjgyc3zanbxmdaqnnqh-note"],"x86_64-linux","/bin/sh",["-c","true"],[("__json","{\"builder\":\"/bin/sh\",\"d\":\"/nix/store/p4pq1jgwyjlyc13nk7vv65a99jzw1843-drvshow-dep-dev\",\"n\":2,\"name\":\"drvshow-top\",\"system\":\"x86_64-linux\",\"t\":\"/nix/store/yb554rm2xvp2vbjgyc3zanbxmdaqnnqh-note\"}"),("out","/nix/store/5railz2b3ssvqdssggdsnga8dyahvbaj-drvshow-top")])"#,
        "drvshow-top",
        r#"{"args":["-c","true"],"builder":"/bin/sh","env":{"out":"/nix/store/5railz2b3ssvqdssggdsnga8dyahvbaj-drvshow-top"},"inputs":{"drvs":{"swp3gmqyvz1k2yjkmncfb29zhdhqfjmn-drvshow-dep.drv":{"dynamicOutputs":{},"outputs":["dev"]}},"srcs":["yb554rm2xvp2vbjgyc3zanbxmdaqnnqh-note"]},"name":"drvshow-top","outputs":{"out":{"path":"5railz2b3ssvqdssggdsnga8dyahvbaj-drvshow-top"}},"structuredAttrs":{"builder":"/bin/sh","d":"/nix/store/p4pq1jgwyjlyc13nk7vv65a99jzw1843-drvshow-dep-dev","n":2,"name":"drvshow-top","system":"x86_64-linux","t":"/nix/store/yb554rm2xvp2vbjgyc3zanbxmdaqnnqh-note"},"system":"x86_64-linux","version":4}"#
    )]
    fn json(#[case] aterm: &str, #[case] name: &str, #[case] want: &str) {
        assert_eq!(json_of(aterm, name), want);
    }

    #[test]
    fn a_non_canonical_structured_attrs_text_is_renormalised() {
        let aterm = r#"Derive([("out","/nix/store/5railz2b3ssvqdssggdsnga8dyahvbaj-drvshow-top","","")],[],[],"x86_64-linux","/bin/sh",[],[("__json","{ \"b\" : 2, \"a\": [1] }"),("out","/nix/store/5railz2b3ssvqdssggdsnga8dyahvbaj-drvshow-top")])"#;
        let got = json_of(aterm, "drvshow-top");
        assert!(
            got.contains(r#""structuredAttrs":{"a":[1],"b":2}"#),
            "{got}"
        );
    }

    /// The forms `nix (Nix) 2.34.8` printed for the same values.
    #[test]
    fn floats_take_nlohmanns_form() {
        let v: Value = serde_json::from_str(
            r#"{"a":1e-5,"b":1e15,"c":1.234e-5,"d":1e-7,"e":2.5,"f":1.0,"g":-0.0,"h":0.0001,"i":1,"j":[1.5,{}],"k":[]}"#,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(json_text(&v, false)).unwrap(),
            "{\"a\":1e-05,\"b\":1e+15,\"c\":1.234e-05,\"d\":1e-07,\"e\":2.5,\"f\":1.0,\"g\":-0.0,\"h\":0.0001,\"i\":1,\"j\":[1.5,{}],\"k\":[]}\n"
        );
    }

    /// `--pretty` must stay byte-equal to what `serde_json` indents.
    #[test]
    fn pretty_matches_serde_json() {
        let v: Value =
            serde_json::from_str(r#"{"a":{"b":[1,"t",{},[],null,true]},"c":"x","d":{},"e":[]}"#)
                .unwrap();
        let mut want = serde_json::to_vec_pretty(&v).unwrap();
        want.push(b'\n');
        assert_eq!(json_text(&v, true), want);
    }
}
