use std::collections::{BTreeMap, BTreeSet};
use std::{error, fmt, fs, mem, str};

use super::hash::{
    Hash, HashAlgo, HashError, HashFormat, base16_encode_into, base16_encode_into_bytes, hash_bytes,
};
use super::path::{
    DRV_EXTENSION, IngestionMethod, STORE_DIR, StorePath, StorePathError, make_fixed_output_path,
    make_output_path, make_text_path, output_path_name,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrvError(pub String);

impl fmt::Display for DrvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl error::Error for DrvError {}

impl From<StorePathError> for DrvError {
    fn from(e: StorePathError) -> DrvError {
        DrvError(e.0)
    }
}

impl From<HashError> for DrvError {
    fn from(e: HashError) -> DrvError {
        DrvError(e.0)
    }
}

/// Mirrors derivations.hh:DerivationOutput (Impure is not supported).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DerivationOutput {
    InputAddressed(StorePath),
    Fixed {
        method: IngestionMethod,
        hash: Hash,
    },
    Deferred,
    CaFloating {
        method: IngestionMethod,
        algo: HashAlgo,
    },
}

/// The env var that carries structured attrs (parsed-derivations.hh:StructuredAttrs::envVarName).
pub const STRUCTURED_ATTRS_ENV: &str = "__json";

/// Mirrors derived-path-map.hh:DerivedPathMap<StringSet>::ChildNode, the value of
/// `input_drvs`. `outputs` are the outputs of that `.drv` this derivation uses.
/// `children` names the outputs that are themselves `.drv`s, and what is used of
/// them: a dependency on the output of a dynamic derivation. It is empty for every
/// derivation that has none, and an empty one writes the traditional `Derive(` ATerm.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputNode {
    pub outputs: BTreeSet<String>,
    pub children: BTreeMap<String, InputNode>,
}

impl InputNode {
    pub fn of(outputs: BTreeSet<String>) -> InputNode {
        InputNode {
            outputs,
            children: BTreeMap::new(),
        }
    }

    fn is_dynamic(&self) -> bool {
        !self.children.is_empty()
    }
}

/// Mirrors derivations.hh:Derivation. `env` must be sorted by key, bytewise, without duplicates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Derivation {
    pub name: String,
    pub outputs: BTreeMap<String, DerivationOutput>,
    pub input_srcs: BTreeSet<StorePath>,
    pub input_drvs: BTreeMap<StorePath, InputNode>,
    pub platform: String,
    pub builder: String,
    pub args: Vec<Vec<u8>>,
    pub env: Vec<(String, Vec<u8>)>,
    pub structured_attrs: Option<String>,
}

/// Mirrors derivations.hh:DrvHash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrvHash {
    pub hashes: BTreeMap<String, Hash>,
    pub kind: DrvHashKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrvHashKind {
    Regular,
    Deferred,
}

/// Memo of `hash_derivation_modulo` per .drv path (derivations.cc:drvHashes).
pub type DrvHashCache = BTreeMap<StorePath, DrvHash>;

/// Reads a .drv that is not in the cache. Mirrors Store::readInvalidDerivation.
pub type DrvLoader<'a> = dyn FnMut(&StorePath) -> Result<Derivation, DrvError> + 'a;

trait PushStr {
    fn push_str(&mut self, s: &str);
}

impl PushStr for Vec<u8> {
    fn push_str(&mut self, s: &str) {
        self.extend_from_slice(s.as_bytes());
    }
}

/// Mirrors derivations.cc:printString: ATerm string with `\"`, `\\`, `\n`, `\r`, `\t` escapes.
fn print_string(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    let mut rest = s;
    while let Some(i) = rest
        .iter()
        .position(|b| matches!(b, b'"' | b'\\' | b'\n' | b'\r' | b'\t'))
    {
        out.extend_from_slice(&rest[..i]);
        out.push_str(match rest[i] {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            _ => "\\t",
        });
        rest = &rest[i + 1..];
    }
    out.extend_from_slice(rest);
    out.push(b'"');
}

/// Mirrors derivations.cc:printUnquotedString (no escaping; for store paths and names).
fn print_unquoted(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    out.push_str(s);
    out.push(b'"');
}

/// `print_unquoted` of `p.to_string()`, without the string.
fn print_store_path(out: &mut Vec<u8>, p: &StorePath) {
    out.push(b'"');
    out.push_str(STORE_DIR);
    out.push(b'/');
    out.push_str(p.base_name());
    out.push(b'"');
}

/// Mirrors content-address.cc:ContentAddress::printMethodAlgo quoted:
/// `"r:sha256"` or `"sha256"`.
fn print_method_algo(out: &mut Vec<u8>, method: IngestionMethod, algo: HashAlgo) {
    out.push(b'"');
    out.push_str(method.prefix());
    out.push_str(algo.name());
    out.push(b'"');
}

/// One ATerm buffer per thread, so the 20,000 unparses of a nixos closure
/// keep the capacity the first one grew to. It is taken out of the slot
/// while in use: `hash_derivation_modulo` recurses, and the inner call must
/// not see the buffer the outer one is filling.
fn with_aterm_buf<R>(f: impl FnOnce(&mut Vec<u8>) -> R) -> R {
    thread_local! {
        static BUF: std::cell::Cell<Vec<u8>> = const { std::cell::Cell::new(Vec::new()) };
    }
    let mut s = BUF.with(|b| b.take());
    s.clear();
    let r = f(&mut s);
    BUF.with(|b| b.set(s));
    r
}

fn print_list<'a>(out: &mut Vec<u8>, items: impl Iterator<Item = &'a [u8]>, quoted: bool) {
    out.push(b'[');
    for (i, s) in items.enumerate() {
        if i > 0 {
            out.push(b',');
        }
        if quoted {
            print_string(out, s);
        } else {
            out.push(b'"');
            out.extend_from_slice(s);
            out.push(b'"');
        }
    }
    out.push(b']');
}

impl DerivationOutput {
    /// Mirrors derivations.cc:DerivationOutput::CAFixed::path.
    pub fn fixed_path(
        &self,
        drv_name: &str,
        output_name: &str,
    ) -> Result<Option<StorePath>, DrvError> {
        match self {
            DerivationOutput::Fixed { method, hash } => Ok(Some(make_fixed_output_path(
                *method,
                hash,
                &output_path_name(drv_name, output_name),
                &BTreeSet::new(),
            )?)),
            _ => Ok(None),
        }
    }

    /// Mirrors derivations.cc:DerivationOutput::path: the statically known output path, if any.
    pub fn path(&self, drv_name: &str, output_name: &str) -> Result<Option<StorePath>, DrvError> {
        match self {
            DerivationOutput::InputAddressed(p) => Ok(Some(p.clone())),
            DerivationOutput::Fixed { .. } => self.fixed_path(drv_name, output_name),
            DerivationOutput::Deferred | DerivationOutput::CaFloating { .. } => Ok(None),
        }
    }
}

impl Derivation {
    /// Mirrors derivations.cc:Derivation::unparse without masking.
    pub fn unparse(&self) -> Result<Vec<u8>, DrvError> {
        self.unparse_with(false, None)
    }

    /// Mirrors derivations.cc:Derivation::unparse(store, maskOutputs, actualInputs).
    pub fn unparse_with(
        &self,
        mask_outputs: bool,
        actual_inputs: Option<&BTreeMap<String, InputNode>>,
    ) -> Result<Vec<u8>, DrvError> {
        let mut s = Vec::with_capacity(4096);
        self.unparse_into(&mut s, mask_outputs, actual_inputs)?;
        Ok(s)
    }

    /// The ATerm of this derivation, then `f` over it. The buffer is reused
    /// between calls, so a caller that only hashes or paths the ATerm and
    /// drops it allocates nothing.
    pub fn with_aterm<R>(&self, f: impl FnOnce(&[u8]) -> R) -> Result<R, DrvError> {
        with_aterm_buf(|s| {
            self.unparse_into(s, false, None)?;
            Ok(f(s))
        })
    }

    /// `unparse_with` appended to a buffer the caller keeps.
    pub fn unparse_into(
        &self,
        s: &mut Vec<u8>,
        mask_outputs: bool,
        actual_inputs: Option<&BTreeMap<String, InputNode>>,
    ) -> Result<(), DrvError> {
        // derivations.cc:hasDynamicDrvDep, over this derivation's own inputs and
        // never over `actual_inputs`, whose nodes the hash-modulo form flattens
        if self.input_drvs.values().any(InputNode::is_dynamic) {
            s.push_str("DrvWithVersion(\"xp-dyn-drv\",[");
        } else {
            s.push_str("Derive([");
        }
        for (i, (name, out)) in self.outputs.iter().enumerate() {
            if i > 0 {
                s.push(b',');
            }
            s.push(b'(');
            print_unquoted(s, name);
            match out {
                DerivationOutput::InputAddressed(p) => {
                    s.push(b',');
                    if mask_outputs {
                        s.push_str("\"\"");
                    } else {
                        print_store_path(s, p);
                    }
                    s.push_str(",\"\",\"\"");
                }
                DerivationOutput::Fixed { method, hash } => {
                    s.push(b',');
                    if mask_outputs {
                        s.push_str("\"\"");
                    } else {
                        print_store_path(s, &self.fixed_output_path(name)?);
                    }
                    s.push(b',');
                    print_method_algo(s, *method, hash.algo);
                    s.push(b',');
                    s.push(b'"');
                    base16_encode_into_bytes(s, hash.bytes());
                    s.push(b'"');
                }
                DerivationOutput::CaFloating { method, algo } => {
                    s.push_str(",\"\",");
                    print_method_algo(s, *method, *algo);
                    s.push_str(",\"\"");
                }
                DerivationOutput::Deferred => s.push_str(",\"\",\"\",\"\""),
            }
            s.push(b')');
        }
        s.push_str("],[");
        match actual_inputs {
            Some(inputs) => {
                for (i, (hash_hex, node)) in inputs.iter().enumerate() {
                    if i > 0 {
                        s.push(b',');
                    }
                    s.push(b'(');
                    print_unquoted(s, hash_hex);
                    unparse_node(s, node);
                    s.push(b')');
                }
            }
            None => {
                for (i, (drv_path, node)) in self.input_drvs.iter().enumerate() {
                    if i > 0 {
                        s.push(b',');
                    }
                    s.push(b'(');
                    print_store_path(s, drv_path);
                    unparse_node(s, node);
                    s.push(b')');
                }
            }
        }
        s.push_str("],[");
        for (i, src) in self.input_srcs.iter().enumerate() {
            if i > 0 {
                s.push(b',');
            }
            print_store_path(s, src);
        }
        s.push_str("],");
        print_unquoted(s, &self.platform);
        s.push(b',');
        print_string(s, self.builder.as_bytes());
        s.push(b',');
        print_list(s, self.args.iter().map(Vec::as_slice), true);
        s.push_str(",[");
        let mut first = true;
        let mut emit = |s: &mut Vec<u8>, k: &str, v: &[u8]| {
            if first {
                first = false;
            } else {
                s.push(b',');
            }
            s.push(b'(');
            print_string(s, k.as_bytes());
            s.push(b',');
            let masked = mask_outputs && self.outputs.contains_key(k);
            print_string(s, if masked { b"" } else { v });
            s.push(b')');
        };
        let mut pending_json = self.structured_attrs.as_deref();
        for (k, v) in &self.env {
            // parsed-derivations.cc:StructuredAttrs::checkKeyNotInUse
            if k == STRUCTURED_ATTRS_ENV {
                return Err(DrvError(format!(
                    "Cannot have an environment variable named '{STRUCTURED_ATTRS_ENV}'. This key is reserved for encoding structured attrs"
                )));
            }
            if let Some(json) = pending_json
                && k.as_str() > STRUCTURED_ATTRS_ENV
            {
                emit(s, STRUCTURED_ATTRS_ENV, json.as_bytes());
                pending_json = None;
            }
            emit(s, k, v);
        }
        if let Some(json) = pending_json {
            emit(s, STRUCTURED_ATTRS_ENV, json.as_bytes());
        }
        s.push_str("])");
        Ok(())
    }

    /// Mirrors derivations.cc:DerivationOutput::CAFixed::path for output `name`.
    pub fn fixed_output_path(&self, output_name: &str) -> Result<StorePath, DrvError> {
        match self.outputs.get(output_name) {
            Some(o) => o
                .fixed_path(&self.name, output_name)?
                .ok_or_else(|| DrvError(format!("output '{output_name}' is not fixed"))),
            None => Err(DrvError(format!("no output '{output_name}'"))),
        }
    }

    /// Mirrors derivations.cc:DerivationType::isFixed: every output is fixed.
    pub fn is_fixed(&self) -> bool {
        !self.outputs.is_empty()
            && self
                .outputs
                .values()
                .all(|o| matches!(o, DerivationOutput::Fixed { .. }))
    }

    /// Mirrors derivations.cc:parseDerivation, both the traditional `Derive(...)`
    /// form and `DrvWithVersion("xp-dyn-drv",...)`.
    pub fn parse(s: &[u8], name: &str) -> Result<Derivation, DrvError> {
        let mut p = Parser { s, pos: 0 };
        let mut drv = Derivation {
            name: name.to_string(),
            ..Default::default()
        };
        let dynamic = if s.starts_with(b"DrvWithVersion(") {
            p.expect(b"DrvWithVersion(")?;
            let v = p.string()?;
            if v != "xp-dyn-drv" {
                return Err(DrvError(format!(
                    "derivation '{name}', ATerm format version '{v}' is not supported"
                )));
            }
            p.expect(b",[")?;
            true
        } else {
            p.expect(b"Derive([")?;
            false
        };
        while !p.end_of_list()? {
            p.expect(b"(")?;
            let id = p.string()?;
            p.expect(b",")?;
            let path = p.string()?;
            p.expect(b",")?;
            let hash_algo = p.string()?;
            p.expect(b",")?;
            let hash = p.string()?;
            p.expect(b")")?;
            drv.outputs
                .insert(id, parse_output(&path, &hash_algo, &hash)?);
        }
        p.expect(b",[")?;
        while !p.end_of_list()? {
            p.expect(b"(")?;
            let drv_path = p.path()?;
            let node = p.input_node(dynamic)?;
            p.expect(b")")?;
            drv.input_drvs.insert(StorePath::parse(&drv_path)?, node);
        }
        p.expect(b",")?;
        for src in p.string_list()? {
            if !src.starts_with('/') {
                return Err(DrvError(format!("bad path '{src}' in derivation")));
            }
            drv.input_srcs.insert(StorePath::parse(&src)?);
        }
        p.expect(b",")?;
        drv.platform = p.string()?;
        p.expect(b",")?;
        drv.builder = p.string()?;
        p.expect(b",[")?;
        while !p.end_of_list()? {
            drv.args.push(p.bytes()?);
        }
        p.expect(b",[")?;
        while !p.end_of_list()? {
            p.expect(b"(")?;
            let k = p.string()?;
            p.expect(b",")?;
            let v = p.bytes()?;
            p.expect(b")")?;
            if k == STRUCTURED_ATTRS_ENV {
                drv.structured_attrs =
                    Some(String::from_utf8(v).map_err(|_| {
                        DrvError("non-UTF-8 structured attrs in derivation".into())
                    })?);
            } else {
                drv.env.push((k, v));
            }
        }
        p.expect(b")")?;
        drv.env.sort_by(|a, b| a.0.cmp(&b.0));
        drv.env.dedup_by(|later, earlier| {
            if later.0 == earlier.0 {
                mem::swap(&mut earlier.1, &mut later.1);
                true
            } else {
                false
            }
        });
        Ok(drv)
    }

    /// Mirrors derivations.cc:BasicDerivation::nameFromPath: strips `.drv` from the path name.
    pub fn name_from_path(drv_path: &StorePath) -> Result<&str, DrvError> {
        drv_path
            .name()
            .strip_suffix(DRV_EXTENSION)
            .ok_or_else(|| DrvError(format!("'{drv_path}' is not a derivation path")))
    }

    #[cfg(test)]
    /// The path of this derivation, for a caller with no ATerm at hand. Mirrors
    /// derivations.cc:computeStorePath. Used by the drv and devenv tests.
    pub fn drv_path(&self) -> Result<StorePath, DrvError> {
        self.with_aterm(|t| self.drv_path_for(t))?
    }

    /// `drv_path` for a caller that already holds `self.unparse()`, so the ATerm is built once.
    pub fn drv_path_for(&self, aterm: &[u8]) -> Result<StorePath, DrvError> {
        let mut references = self.input_srcs.clone();
        references.extend(self.input_drvs.keys().cloned());
        let hash = hash_bytes(HashAlgo::Sha256, aterm);
        let mut name = String::with_capacity(self.name.len() + DRV_EXTENSION.len());
        name.push_str(&self.name);
        name.push_str(DRV_EXTENSION);
        Ok(make_text_path(&name, &hash, &references)?)
    }

    /// Looks up `key` in the sorted env.
    pub fn env_get(&self, key: &str) -> Option<&[u8]> {
        self.env
            .binary_search_by(|(k, _)| k.as_str().cmp(key))
            .ok()
            .map(|i| self.env[i].1.as_slice())
    }

    /// `env_get` of a value that is UTF-8; `None` for one that is not.
    pub fn env_str(&self, key: &str) -> Option<&str> {
        self.env_get(key).and_then(|v| str::from_utf8(v).ok())
    }

    /// Inserts or replaces `key` in the sorted env.
    pub fn env_set(&mut self, key: &str, value: Vec<u8>) {
        match self.env.binary_search_by(|(k, _)| k.as_str().cmp(key)) {
            Ok(i) => self.env[i].1 = value,
            Err(i) => self.env.insert(i, (key.to_string(), value)),
        }
    }

    /// Removes `key` from the sorted env, if present.
    pub fn env_remove(&mut self, key: &str) {
        if let Ok(i) = self.env.binary_search_by(|(k, _)| k.as_str().cmp(key)) {
            self.env.remove(i);
        }
    }
}

/// Mirrors derivations.cc:parseDerivationOutput(store, pathS, hashAlgoStr, hashS).
fn parse_output(path: &str, hash_algo: &str, hash: &str) -> Result<DerivationOutput, DrvError> {
    if hash_algo.is_empty() {
        if path.is_empty() {
            return Ok(DerivationOutput::Deferred);
        }
        return Ok(DerivationOutput::InputAddressed(StorePath::parse(path)?));
    }
    let (method, algo_name) = if let Some(rest) = hash_algo.strip_prefix("r:") {
        (IngestionMethod::Recursive, rest)
    } else if let Some(rest) = hash_algo.strip_prefix("text:") {
        (IngestionMethod::Text, rest)
    } else if hash_algo.starts_with("git:") {
        return Err(DrvError(format!(
            "unsupported output hash method '{hash_algo}'"
        )));
    } else {
        (IngestionMethod::Flat, hash_algo)
    };
    let algo = HashAlgo::parse(algo_name)
        .ok_or_else(|| DrvError(format!("unknown hash algorithm '{algo_name}'")))?;
    if hash == "impure" {
        return Err(DrvError("impure derivations are not supported".into()));
    }
    if hash.is_empty() {
        if !path.is_empty() {
            return Err(DrvError(
                "content-addressing derivation output should not specify output path".into(),
            ));
        }
        return Ok(DerivationOutput::CaFloating { method, algo });
    }
    if !path.starts_with('/') {
        return Err(DrvError(format!("bad path '{path}' in derivation")));
    }
    let hash = Hash::parse_non_sri_unprefixed(hash, algo)?;
    Ok(DerivationOutput::Fixed { method, hash })
}

struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn expect(&mut self, lit: &[u8]) -> Result<(), DrvError> {
        if self.s[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(())
        } else {
            Err(DrvError(format!(
                "expected string '{}' at offset {}",
                String::from_utf8_lossy(lit),
                self.pos
            )))
        }
    }

    /// Mirrors derivations.cc:endOfList.
    fn end_of_list(&mut self) -> Result<bool, DrvError> {
        match self.peek() {
            Some(b',') => {
                self.pos += 1;
                Ok(false)
            }
            Some(b']') => {
                self.pos += 1;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn string(&mut self) -> Result<String, DrvError> {
        String::from_utf8(self.bytes()?)
            .map_err(|_| DrvError("non-UTF-8 string in derivation".into()))
    }

    /// Mirrors derivations.cc:parseString.
    fn bytes(&mut self) -> Result<Vec<u8>, DrvError> {
        self.expect(b"\"")?;
        let mut out = Vec::new();
        loop {
            let c = *self
                .s
                .get(self.pos)
                .ok_or_else(|| DrvError("unterminated string in derivation".into()))?;
            self.pos += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = *self
                        .s
                        .get(self.pos)
                        .ok_or_else(|| DrvError("unterminated string in derivation".into()))?;
                    self.pos += 1;
                    out.push(match e {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        e => e,
                    });
                }
                c => out.push(c),
            }
        }
        Ok(out)
    }

    /// Mirrors derivations.cc:parsePath.
    fn path(&mut self) -> Result<String, DrvError> {
        let s = self.string()?;
        if !s.starts_with('/') {
            return Err(DrvError(format!("bad path '{s}' in derivation")));
        }
        Ok(s)
    }

    /// Mirrors derivations.cc:parseStrings.
    fn string_list(&mut self) -> Result<BTreeSet<String>, DrvError> {
        let mut res = BTreeSet::new();
        self.expect(b"[")?;
        while !self.end_of_list()? {
            res.insert(self.string()?);
        }
        Ok(res)
    }

    /// Mirrors derivations.cc:parseDerivedPathMapNode. The leading comma is the
    /// one after the `.drv` path or the output name. An `xp-dyn-drv` derivation
    /// may still write the flat form for an input that has no dynamic output.
    fn input_node(&mut self, dynamic: bool) -> Result<InputNode, DrvError> {
        self.expect(b",")?;
        if !dynamic || self.peek() == Some(b'[') {
            return Ok(InputNode::of(self.string_list()?));
        }
        if self.peek() != Some(b'(') {
            return Err(DrvError("invalid inputDrvs entry in derivation".into()));
        }
        self.expect(b"(")?;
        let outputs = self.string_list()?;
        self.expect(b",[")?;
        let mut children = BTreeMap::new();
        while !self.end_of_list()? {
            self.expect(b"(")?;
            let name = self.string()?;
            let child = self.input_node(dynamic)?;
            self.expect(b")")?;
            children.insert(name, child);
        }
        self.expect(b")")?;
        Ok(InputNode { outputs, children })
    }
}

/// Mirrors derivations.cc:unparseDerivedPathMapNode, including the comma it writes.
fn unparse_node(s: &mut Vec<u8>, node: &InputNode) {
    s.push(b',');
    if node.children.is_empty() {
        print_list(s, node.outputs.iter().map(String::as_bytes), false);
        return;
    }
    s.push(b'(');
    print_list(s, node.outputs.iter().map(String::as_bytes), false);
    s.push_str(",[");
    for (i, (name, child)) in node.children.iter().enumerate() {
        if i > 0 {
            s.push(b',');
        }
        s.push(b'(');
        print_unquoted(s, name);
        unparse_node(s, child);
        s.push(b')');
    }
    s.push_str("])");
}

/// Mirrors derivations.cc:hashDerivationModulo. `loader` reads .drv files that are not cached.
pub fn hash_derivation_modulo(
    drv: &Derivation,
    mask_outputs: bool,
    cache: &mut DrvHashCache,
    loader: &mut DrvLoader<'_>,
) -> Result<DrvHash, DrvError> {
    if drv.is_fixed() {
        let mut hashes = BTreeMap::new();
        let mut payload = String::with_capacity(256);
        for (name, out) in &drv.outputs {
            let DerivationOutput::Fixed { method, hash } = out else {
                unreachable!()
            };
            payload.clear();
            payload.push_str("fixed:out:");
            payload.push_str(method.prefix());
            payload.push_str(hash.algo.name());
            payload.push(':');
            base16_encode_into(&mut payload, hash.bytes());
            payload.push(':');
            payload.push_str(STORE_DIR);
            payload.push('/');
            payload.push_str(drv.fixed_output_path(name)?.base_name());
            hashes.insert(
                name.clone(),
                hash_bytes(HashAlgo::Sha256, payload.as_bytes()),
            );
        }
        return Ok(DrvHash {
            hashes,
            kind: DrvHashKind::Regular,
        });
    }

    let mut kind = if drv
        .outputs
        .values()
        .any(|o| matches!(o, DerivationOutput::CaFloating { .. }))
    {
        DrvHashKind::Deferred
    } else {
        DrvHashKind::Regular
    };

    // nix drops `children` here: only the direct output names of each input go
    // through the modulo hash, so the nodes of `inputs2` are always flat
    let mut inputs2: BTreeMap<String, InputNode> = BTreeMap::new();
    let mut hex = String::with_capacity(128);
    for (drv_path, node) in &drv.input_drvs {
        let res = path_derivation_modulo(drv_path, cache, loader)?;
        if res.kind == DrvHashKind::Deferred {
            kind = DrvHashKind::Deferred;
        }
        for output in &node.outputs {
            let h = res.hashes.get(output).ok_or_else(|| {
                DrvError(format!(
                    "no hash for output '{output}' of derivation '{}'",
                    drv.name
                ))
            })?;
            hex.clear();
            h.write_string_with(&mut hex, HashFormat::Base16, false);
            // the same modulo hash covers every output of one input
            match inputs2.get_mut(hex.as_str()) {
                Some(v) => v.outputs.insert(output.clone()),
                None => inputs2
                    .entry(hex.clone())
                    .or_default()
                    .outputs
                    .insert(output.clone()),
            };
        }
    }

    let hash = with_aterm_buf(|s| {
        drv.unparse_into(s, mask_outputs, Some(&inputs2))?;
        Ok::<_, DrvError>(hash_bytes(HashAlgo::Sha256, s))
    })?;
    let hashes = drv.outputs.keys().map(|k| (k.clone(), hash)).collect();
    Ok(DrvHash { hashes, kind })
}

/// Mirrors derivations.cc:pathDerivationModulo (memoized by .drv path).
pub fn path_derivation_modulo(
    drv_path: &StorePath,
    cache: &mut DrvHashCache,
    loader: &mut DrvLoader<'_>,
) -> Result<DrvHash, DrvError> {
    if let Some(h) = cache.get(drv_path) {
        return Ok(h.clone());
    }
    let drv = loader(drv_path)?;
    let h = hash_derivation_modulo(&drv, false, cache, loader)?;
    cache.insert(drv_path.clone(), h.clone());
    Ok(h)
}

/// Reads and parses `/nix/store/<hash>-<name>.drv`. Mirrors Store::readInvalidDerivation for a
/// local store; the default `DrvLoader`.
pub fn load_derivation_from_store(drv_path: &StorePath) -> Result<Derivation, DrvError> {
    let name = Derivation::name_from_path(drv_path)?;
    let full = format!("{STORE_DIR}/{}", drv_path.base_name());
    let text =
        fs::read(&full).map_err(|e| DrvError(format!("cannot read derivation '{full}': {e}")))?;
    Derivation::parse(&text, name)
}

/// Mirrors derivations.cc:Derivation::fillInOutputPaths as used by primops.cc:derivationStrict:
/// Deferred outputs become input-addressed paths from the masked hash modulo, and the env
/// entries named after the outputs are set to the paths (fixed outputs included).
pub fn finalize_input_addressed_outputs(
    drv: &mut Derivation,
    cache: &mut DrvHashCache,
    loader: &mut DrvLoader<'_>,
) -> Result<(), DrvError> {
    let mut hashes_modulo: Option<DrvHash> = None;
    let names: Vec<String> = drv.outputs.keys().cloned().collect();
    for name in names {
        let out = drv.outputs[&name].clone();
        let actual = match out {
            DerivationOutput::Fixed { .. } => drv.fixed_output_path(&name)?,
            DerivationOutput::InputAddressed(_) | DerivationOutput::Deferred => {
                let hm = match hashes_modulo {
                    Some(ref h) => h,
                    None => hashes_modulo.insert(hash_derivation_modulo(drv, true, cache, loader)?),
                };
                match hm.kind {
                    DrvHashKind::Deferred => {
                        if let DerivationOutput::InputAddressed(p) = out {
                            return Err(DrvError(format!(
                                "derivation has incorrect output '{p}', should be deferred"
                            )));
                        }
                        continue;
                    }
                    DrvHashKind::Regular => {
                        let h = hm.hashes.get(&name).ok_or_else(|| {
                            DrvError(format!("derivation produced no hash for output '{name}'"))
                        })?;
                        let path = make_output_path(&name, h, &drv.name)?;
                        if let DerivationOutput::InputAddressed(p) = &out
                            && *p != path
                        {
                            return Err(DrvError(format!(
                                "derivation has incorrect output '{p}', should be '{path}'"
                            )));
                        }
                        drv.outputs
                            .insert(name.clone(), DerivationOutput::InputAddressed(path.clone()));
                        path
                    }
                }
            }
            DerivationOutput::CaFloating { .. } => continue,
        };
        let actual = actual.to_string();
        match drv.env_get(&name) {
            None | Some(b"") => drv.env_set(&name, actual.into_bytes()),
            Some(v) if v != actual.as_bytes() => {
                return Err(DrvError(format!(
                    "derivation has incorrect environment variable '{name}', should be '{actual}' but is actually '{}'",
                    String::from_utf8_lossy(v)
                )));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn fixture_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/drv")
    }

    /// Copies of /nix/store/*.drv produced by Nix 2.34.8 (`cp /nix/store/<name> tests/fixtures/drv/`).
    /// The file name is the store path, so `drv_path()` is checked against it.
    fn fixtures() -> Vec<(StorePath, Vec<u8>)> {
        let mut v: Vec<(StorePath, Vec<u8>)> = fs::read_dir(fixture_dir())
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let base = e.file_name().into_string().unwrap();
                (
                    StorePath::from_base_name(&base).unwrap(),
                    fs::read(e.path()).unwrap(),
                )
            })
            .collect();
        v.sort();
        assert!(
            v.len() >= 30,
            "expected at least 30 fixtures, got {}",
            v.len()
        );
        v
    }

    fn fixture_loader(drv_path: &StorePath) -> Result<Derivation, DrvError> {
        let local = fixture_dir().join(drv_path.base_name());
        if local.exists() {
            let text = fs::read(local).unwrap();
            return Derivation::parse(&text, Derivation::name_from_path(drv_path)?);
        }
        load_derivation_from_store(drv_path)
    }

    #[test]
    fn fixtures_roundtrip_byte_exact() {
        let mut fixed = 0;
        let mut multi = 0;
        let mut json = 0;
        for (path, text) in fixtures() {
            let drv = Derivation::parse(&text, Derivation::name_from_path(&path).unwrap()).unwrap();
            assert_eq!(drv.unparse().unwrap(), text, "unparse mismatch for {path}");
            fixed += usize::from(drv.is_fixed());
            multi += usize::from(drv.outputs.len() > 1);
            json += usize::from(drv.structured_attrs.is_some());
        }
        assert!(fixed >= 8, "fixed-output fixtures: {fixed}");
        assert!(multi >= 10, "multi-output fixtures: {multi}");
        assert!(json >= 3, "structured-attrs fixtures: {json}");
    }

    #[test]
    fn fixtures_drv_path_matches_store_path() {
        for (path, text) in fixtures() {
            let drv = Derivation::parse(&text, Derivation::name_from_path(&path).unwrap()).unwrap();
            assert_eq!(
                drv.drv_path().unwrap(),
                path,
                "drv_path mismatch for {path}"
            );
        }
    }

    /// The whole input closure of `drv` is vendored under tests/fixtures/drv.
    fn closure_in_fixtures(drv: &Derivation) -> bool {
        drv.input_drvs.keys().all(|p| {
            let Ok(text) = fs::read(fixture_dir().join(p.base_name())) else {
                return false;
            };
            Derivation::name_from_path(p)
                .and_then(|n| Derivation::parse(&text, n))
                .is_ok_and(|d| closure_in_fixtures(&d))
        })
    }

    /// Resets every input-addressed output to Deferred with an empty env value, as
    /// primops.cc:derivationStrict does, then checks that finalize reproduces the original.
    /// Fixtures whose input .drv closure `present` rejects are skipped.
    fn recompute_fixtures(present: impl Fn(&Derivation) -> bool) -> usize {
        let mut cache = DrvHashCache::new();
        let mut loader = fixture_loader;
        let mut checked = 0;
        for (path, text) in fixtures() {
            let original =
                Derivation::parse(&text, Derivation::name_from_path(&path).unwrap()).unwrap();
            if !present(&original) {
                continue;
            }
            let mut drv = original.clone();
            for (name, out) in drv.outputs.iter_mut() {
                if matches!(out, DerivationOutput::InputAddressed(_)) {
                    *out = DerivationOutput::Deferred;
                    drv.env
                        .iter_mut()
                        .find(|(k, _)| k == name)
                        .unwrap()
                        .1
                        .clear();
                }
            }
            finalize_input_addressed_outputs(&mut drv, &mut cache, &mut loader).unwrap();
            assert_eq!(drv, original, "output paths differ for {path}");
            checked += 1;
        }
        checked
    }

    #[test]
    fn fixtures_output_paths_recompute() {
        let n = recompute_fixtures(closure_in_fixtures);
        assert!(n >= 7, "recomputed only {n} derivations");
    }

    /// The nixpkgs fixtures have closures of hundreds of .drv files, too many to vendor.
    #[test]
    #[ignore = "needs the input .drv closures in /nix/store"]
    fn fixtures_output_paths_recompute_from_store() {
        let n = recompute_fixtures(|drv| {
            drv.input_drvs.keys().all(|p| {
                Path::new(&p.to_string()).exists() || fixture_dir().join(p.base_name()).exists()
            })
        });
        assert!(n >= 20, "recomputed only {n} derivations");
    }

    /// derivations.cc:unparseDerivedPathMapNode and parseDerivedPathMapNode. An
    /// input with children puts the whole derivation in the versioned form, and
    /// one without keeps the flat node even there.
    #[test]
    fn a_dynamic_input_writes_the_versioned_aterm_and_parses_back() {
        let dep = StorePath::parse("/nix/store/00000000000000000000000000000000-dep.drv").unwrap();
        let plain =
            StorePath::parse("/nix/store/11111111111111111111111111111111-plain.drv").unwrap();
        let mut drv = simple("dyn", &["out"]);
        drv.input_drvs
            .insert(plain, InputNode::of(BTreeSet::from(["out".to_string()])));
        drv.input_drvs.insert(
            dep,
            InputNode {
                outputs: BTreeSet::from(["out".to_string()]),
                children: BTreeMap::from([(
                    "out".to_string(),
                    InputNode {
                        outputs: BTreeSet::from(["out".to_string()]),
                        children: BTreeMap::from([(
                            "out".to_string(),
                            InputNode::of(BTreeSet::from(["out".to_string()])),
                        )]),
                    },
                )]),
            },
        );
        let aterm = String::from_utf8(drv.unparse().unwrap()).unwrap();
        assert!(
            aterm.starts_with("DrvWithVersion(\"xp-dyn-drv\",["),
            "{aterm}"
        );
        assert!(
            aterm.contains(
                "(\"/nix/store/00000000000000000000000000000000-dep.drv\",\
                 ([\"out\"],[(\"out\",([\"out\"],[(\"out\",[\"out\"])]))]))"
            ),
            "{aterm}"
        );
        assert!(
            aterm.contains("(\"/nix/store/11111111111111111111111111111111-plain.drv\",[\"out\"])"),
            "{aterm}"
        );
        assert_eq!(Derivation::parse(aterm.as_bytes(), "dyn").unwrap(), drv);

        // without children the traditional form comes back
        drv.input_drvs.retain(|p, _| p.name() == "plain.drv");
        let flat = String::from_utf8(drv.unparse().unwrap()).unwrap();
        assert!(flat.starts_with("Derive(["), "{flat}");
        assert_eq!(Derivation::parse(flat.as_bytes(), "dyn").unwrap(), drv);
    }

    fn simple(name: &str, outputs: &[&str]) -> Derivation {
        let mut drv = Derivation {
            name: name.into(),
            platform: "x86_64-linux".into(),
            builder: "/bin/sh".into(),
            ..Default::default()
        };
        for o in outputs {
            drv.outputs
                .insert(o.to_string(), DerivationOutput::Deferred);
            drv.env_set(o, Vec::new());
        }
        drv.env_set("builder", "/bin/sh".into());
        drv.env_set("name", name.into());
        drv.env_set("system", "x86_64-linux".into());
        drv
    }

    // nix-instantiate drvs.nix -A ia, with
    //   ia = builtins.derivation { name = "ia"; system = "x86_64-linux"; builder = "/bin/sh";
    //     args = ["-c" "x\"y\\z\n"]; outputs = ["out" "dev"]; FOO = "b\tar"; };
    #[test]
    fn input_addressed_multi_output_from_scratch() {
        let mut drv = simple("ia", &["out", "dev"]);
        drv.args = vec!["-c".into(), "x\"y\\z\n".into()];
        drv.env_set("FOO", "b\tar".into());
        drv.env_set("outputs", "out dev".into());
        let mut cache = DrvHashCache::new();
        finalize_input_addressed_outputs(&mut drv, &mut cache, &mut |p| {
            Err(DrvError(format!("unexpected load of {p}")))
        })
        .unwrap();
        assert_eq!(
            drv.drv_path().unwrap().to_string(),
            "/nix/store/73as87hfqgllqymcyhn1bi8iman2wwqx-ia.drv"
        );
        assert_eq!(
            drv.outputs["out"],
            DerivationOutput::InputAddressed(
                StorePath::parse("/nix/store/0rzxv3xkkgply9jd76fdiwfm6k1gyj6p-ia").unwrap()
            )
        );
        assert_eq!(
            drv.outputs["dev"],
            DerivationOutput::InputAddressed(
                StorePath::parse("/nix/store/nx6zsacidiymqhs3yji2j0i0srcnnvkc-ia-dev").unwrap()
            )
        );
        assert_eq!(
            drv.env_str("dev"),
            Some("/nix/store/nx6zsacidiymqhs3yji2j0i0srcnnvkc-ia-dev")
        );
        let expected =
            fs::read(fixture_dir().join("73as87hfqgllqymcyhn1bi8iman2wwqx-ia.drv")).unwrap();
        assert_eq!(drv.unparse().unwrap(), expected);
    }

    // nix-instantiate drvs.nix -A fr, with
    //   fr = builtins.derivation { name = "fr"; system = "x86_64-linux"; builder = "/bin/sh";
    //     outputHash = "ba78...15ad"; outputHashAlgo = "sha256"; outputHashMode = "recursive"; };
    #[test]
    fn fixed_output_from_scratch() {
        let hex = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let mut drv = simple("fr", &[]);
        drv.outputs.insert(
            "out".into(),
            DerivationOutput::Fixed {
                method: IngestionMethod::Recursive,
                hash: Hash::parse_any(hex, Some(HashAlgo::Sha256)).unwrap(),
            },
        );
        drv.env_set("outputHash", hex.into());
        drv.env_set("outputHashAlgo", "sha256".into());
        drv.env_set("outputHashMode", "recursive".into());
        let mut cache = DrvHashCache::new();
        finalize_input_addressed_outputs(&mut drv, &mut cache, &mut |p| {
            Err(DrvError(format!("unexpected load of {p}")))
        })
        .unwrap();
        assert_eq!(
            drv.env_str("out"),
            Some("/nix/store/s05rl80bpix1186wb5f471k7fxwwp7lf-fr")
        );
        assert_eq!(
            drv.drv_path().unwrap().to_string(),
            "/nix/store/sqjiq38d04j09g4dws6a9d644618zf21-fr.drv"
        );
        let expected =
            fs::read(fixture_dir().join("sqjiq38d04j09g4dws6a9d644618zf21-fr.drv")).unwrap();
        assert_eq!(drv.unparse().unwrap(), expected);
    }

    // nix-instantiate drvs.nix -A dep2, with
    //   dep = builtins.derivation { name = "dep"; system = "x86_64-linux"; builder = "/bin/sh";
    //     f = fx; src = builtins.toFile "a" "x"; };
    //   dep2 = builtins.derivation { name = "dep2"; system = "x86_64-linux"; builder = "/bin/sh";
    //     d = ia.dev; e = "${dep}"; };
    // This exercises the fixed-output special case (fx) and the recursion through the cache.
    #[test]
    fn input_drvs_hash_modulo_from_scratch() {
        let ia = StorePath::parse("/nix/store/73as87hfqgllqymcyhn1bi8iman2wwqx-ia.drv").unwrap();
        let dep = StorePath::parse("/nix/store/wgj4nv00ip0zkjhgs6bq3f6yrfikw0m5-dep.drv").unwrap();
        let mut drv = simple("dep2", &["out"]);
        drv.env_set(
            "d",
            "/nix/store/nx6zsacidiymqhs3yji2j0i0srcnnvkc-ia-dev".into(),
        );
        drv.env_set(
            "e",
            "/nix/store/pzjq1hdf1x9qs1dxcqiba82dyl1274aa-dep".into(),
        );
        drv.input_drvs
            .insert(ia, InputNode::of(["dev".to_string()].into_iter().collect()));
        drv.input_drvs.insert(
            dep.clone(),
            InputNode::of(["out".to_string()].into_iter().collect()),
        );
        let mut cache = DrvHashCache::new();
        finalize_input_addressed_outputs(&mut drv, &mut cache, &mut fixture_loader).unwrap();
        assert_eq!(
            drv.drv_path().unwrap().to_string(),
            "/nix/store/0fdrgjkjrlf9vi5xgyxfl6s8ihav2idi-dep2.drv"
        );
        assert_eq!(
            drv.env_str("out"),
            Some("/nix/store/f1lfacbx1gq8g6pw0290cijhli2n3nls-dep2")
        );
        assert!(cache.contains_key(&dep));
        assert!(cache.contains_key(
            &StorePath::parse("/nix/store/pckkdf72dvcpi4l4cbg668xbfvyg4ps8-fx.drv").unwrap()
        ));
    }

    #[test]
    fn string_escapes_roundtrip() {
        let mut s = Vec::new();
        print_string(&mut s, b"a\"b\\c\nd\re\tf$g");
        assert_eq!(s, b"\"a\\\"b\\\\c\\nd\\re\\tf$g\"");
        let mut p = Parser { s: &s, pos: 0 };
        assert_eq!(p.string().unwrap(), "a\"b\\c\nd\re\tf$g");
        assert!(
            Parser {
                s: b"\"abc",
                pos: 0
            }
            .string()
            .is_err()
        );
        assert!(
            Parser {
                s: b"\"abc\\",
                pos: 0
            }
            .string()
            .is_err()
        );
    }

    #[test]
    fn masked_unparse_blanks_output_paths_and_env() {
        let text = fs::read(fixture_dir().join("73as87hfqgllqymcyhn1bi8iman2wwqx-ia.drv")).unwrap();
        let drv = Derivation::parse(&text, "ia").unwrap();
        let masked = String::from_utf8(drv.unparse_with(true, None).unwrap()).unwrap();
        assert!(masked.starts_with("Derive([(\"dev\",\"\",\"\",\"\"),(\"out\",\"\",\"\",\"\")],"));
        assert!(masked.contains("(\"dev\",\"\")"));
        assert!(masked.contains("(\"out\",\"\")"));
        assert!(!masked.contains("/nix/store/0rzxv3xkkgply9jd76fdiwfm6k1gyj6p-ia"));
    }

    #[test]
    fn an_env_key_named_json_is_refused() {
        let mut drv = Derivation {
            name: "o1".into(),
            platform: "x".into(),
            builder: "/bin/sh".into(),
            ..Default::default()
        };
        drv.env_set("__json", "/nix/store/x".into());
        let e = drv.with_aterm(|_| ()).unwrap_err();
        assert_eq!(
            e.0,
            "Cannot have an environment variable named '__json'. This key is reserved for encoding structured attrs"
        );
    }
}
