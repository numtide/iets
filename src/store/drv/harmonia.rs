//! `Derivation::parse` on harmonia-store-aterm (github.com/nix-community/harmonia, MIT).
//! harmonia owns its types, so the result is converted into ours here.

use std::collections::BTreeMap;

use harmonia_store_aterm::parse_derivation_aterm;
use harmonia_store_content_address::{ContentAddressMethod, ContentAddressMethodAlgorithm};
use harmonia_store_derivation::derivation::{
    DerivationInputs, DerivationOutput as HOutput, OutputInputs,
};
use harmonia_store_path::{StoreDir, StorePath as HStorePath, StorePathName};

use super::{Derivation, DerivationOutput, DrvError, InputNode};
use crate::store::hash::{Hash, HashAlgo};
use crate::store::path::{IngestionMethod, StorePath, store_dir};

pub(super) fn parse(s: &[u8], name: &str) -> Result<Derivation, DrvError> {
    let dir = StoreDir::new(store_dir()).map_err(|e| DrvError(e.to_string()))?;
    let hname = StorePathName::try_from(name.as_bytes()).map_err(|e| DrvError(e.to_string()))?;
    let h = parse_derivation_aterm(&dir, s, hname).map_err(|e| DrvError(e.to_string()))?;
    let inputs = DerivationInputs::from(&h.inputs);
    let mut outputs = BTreeMap::new();
    for (id, o) in &h.outputs {
        outputs.insert(id.as_ref().to_string(), output(o)?);
    }
    let mut env = Vec::with_capacity(h.env.len());
    for (k, v) in h.env {
        env.push((utf8(k.to_vec())?, v.to_vec()));
    }
    Ok(Derivation {
        name: name.to_string(),
        outputs,
        input_srcs: inputs.srcs.iter().map(path).collect::<Result<_, _>>()?,
        input_drvs: inputs
            .drvs
            .iter()
            .map(|(p, n)| Ok((path(p)?, node(n))))
            .collect::<Result<_, DrvError>>()?,
        platform: utf8(h.platform.to_vec())?,
        builder: utf8(h.builder.to_vec())?,
        args: h.args.iter().map(|a| a.to_vec()).collect(),
        env,
        structured_attrs: h
            .structured_attrs
            .map(|sa| serde_json::to_string(&sa.attrs).map_err(|e| DrvError(e.to_string())))
            .transpose()?,
    })
}

fn utf8(b: Vec<u8>) -> Result<String, DrvError> {
    String::from_utf8(b).map_err(|_| DrvError("non-UTF-8 string in derivation".into()))
}

fn path(p: &HStorePath) -> Result<StorePath, DrvError> {
    Ok(StorePath::from_base_name(&p.to_string())?)
}

fn node(n: &OutputInputs) -> InputNode {
    InputNode {
        outputs: n.outputs.iter().map(|o| o.as_ref().to_string()).collect(),
        children: n
            .dynamic_outputs
            .iter()
            .map(|(o, c)| (o.as_ref().to_string(), node(c)))
            .collect(),
    }
}

fn method(m: ContentAddressMethod) -> IngestionMethod {
    match m {
        ContentAddressMethod::Text => IngestionMethod::Text,
        ContentAddressMethod::Flat => IngestionMethod::Flat,
        ContentAddressMethod::NixArchive => IngestionMethod::Recursive,
    }
}

fn algo(ma: ContentAddressMethodAlgorithm) -> Result<HashAlgo, DrvError> {
    let name = ma.algorithm().to_string();
    HashAlgo::parse(&name).ok_or_else(|| DrvError(format!("unknown hash algorithm '{name}'")))
}

fn output(o: &HOutput) -> Result<DerivationOutput, DrvError> {
    Ok(match o {
        HOutput::InputAddressed(p) => DerivationOutput::InputAddressed(path(p)?),
        HOutput::Deferred => DerivationOutput::Deferred,
        HOutput::CAFloating(ma) => DerivationOutput::CaFloating {
            method: method(ma.method()),
            algo: algo(*ma)?,
        },
        HOutput::CAFixed(ca) => DerivationOutput::Fixed {
            method: method(ca.method()),
            hash: Hash::from_bytes(algo(ca.method_algorithm())?, ca.hash().digest_bytes())?,
        },
        HOutput::Impure(_) => return Err(DrvError("impure derivations are not supported".into())),
    })
}
