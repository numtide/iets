use std::collections::BTreeSet;
use std::{error, fmt};

use super::hash::{self, Hash, HashAlgo, HashFormat, compress_hash, hash_bytes, is_nix32_char};

use crate::events::{self, Field};

pub const STORE_DIR: &str = "/nix/store";

/// Mirrors path.hh:StorePath::HashLen (base32 characters).
pub const HASH_PART_LEN: usize = 32;

/// Mirrors path.hh:StorePath::MaxPathLen.
pub const MAX_NAME_LEN: usize = 211;

pub const DRV_EXTENSION: &str = ".drv";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorePathError(pub String);

impl fmt::Display for StorePathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl error::Error for StorePathError {}

/// A store path stored as its base name `<hash>-<name>`, like path.hh:StorePath.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StorePath {
    base_name: String,
}

impl fmt::Debug for StorePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StorePath({})", self.base_name)
    }
}

impl fmt::Display for StorePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{STORE_DIR}/{}", self.base_name)
    }
}

/// Mirrors path.cc:checkName.
pub fn check_name(name: &str) -> Result<(), StorePathError> {
    if name.is_empty() {
        return Err(StorePathError("name must not be empty".into()));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(StorePathError(format!(
            "name '{name}' must be no longer than {MAX_NAME_LEN} characters"
        )));
    }
    let b = name.as_bytes();
    if b[0] == b'.' {
        if b.len() == 1 {
            return Err(StorePathError(format!("name '{name}' is not valid")));
        }
        if b[1] == b'-' {
            return Err(StorePathError(format!(
                "name '{name}' is not valid: first dash-separated component must not be '.'"
            )));
        }
        if b[1] == b'.' {
            if b.len() == 2 {
                return Err(StorePathError(format!("name '{name}' is not valid")));
            }
            if b[2] == b'-' {
                return Err(StorePathError(format!(
                    "name '{name}' is not valid: first dash-separated component must not be '..'"
                )));
            }
        }
    }
    for &c in b {
        if !(c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.' | b'_' | b'?' | b'=')) {
            return Err(StorePathError(format!(
                "name '{name}' contains illegal character '{}'",
                c as char
            )));
        }
    }
    Ok(())
}

impl StorePath {
    /// Mirrors path.cc:StorePath::StorePath(const Hash &, std::string_view name).
    pub fn new(hash: &Hash, name: &str) -> Result<StorePath, StorePathError> {
        check_name(name)?;
        let mut base_name = String::with_capacity(HASH_PART_LEN + 1 + name.len());
        hash::nix32_encode_into(&mut base_name, hash.bytes());
        debug_assert_eq!(base_name.len(), HASH_PART_LEN);
        base_name.push('-');
        base_name.push_str(name);
        Ok(StorePath { base_name })
    }

    /// Mirrors path.cc:StorePath::StorePath(std::string_view baseName): `<hash>-<name>`.
    pub fn from_base_name(base_name: &str) -> Result<StorePath, StorePathError> {
        let bad = |why: &str| {
            StorePathError(format!(
                "path '{base_name}' is not a valid store path: {why}"
            ))
        };
        if base_name.len() < HASH_PART_LEN + 1 {
            return Err(bad("path too short"));
        }
        let (hash_part, rest) = base_name.split_at(HASH_PART_LEN);
        if !rest.starts_with('-') {
            return Err(bad("missing dash"));
        }
        if let Some(c) = hash_part.bytes().find(|&c| !is_nix32_char(c)) {
            return Err(bad(&format!(
                "illegal base-32 character '{}' in the hash part",
                c as char
            )));
        }
        check_name(&rest[1..]).map_err(|e| bad(&e.0))?;
        Ok(StorePath {
            base_name: base_name.to_string(),
        })
    }

    /// Mirrors store-api.cc:StoreDirConfig::parseStorePath: `/nix/store/<hash>-<name>`.
    pub fn parse(path: &str) -> Result<StorePath, StorePathError> {
        let bad = || StorePathError(format!("path '{path}' is not in the Nix store"));
        let rest = path.strip_prefix(STORE_DIR).ok_or_else(bad)?;
        let rest = rest.strip_prefix('/').ok_or_else(bad)?;
        if rest.contains('/') {
            return Err(bad());
        }
        Self::from_base_name(rest)
    }

    pub fn base_name(&self) -> &str {
        &self.base_name
    }

    pub fn hash_part(&self) -> &str {
        &self.base_name[..HASH_PART_LEN]
    }

    pub fn name(&self) -> &str {
        &self.base_name[HASH_PART_LEN + 1..]
    }

    /// Mirrors path.hh:StorePath::isDerivation.
    pub fn is_derivation(&self) -> bool {
        self.name().ends_with(DRV_EXTENSION)
    }

    /// The full path `/nix/store/<hash>-<name>` (store-api.cc:printStorePath).
    pub fn to_full_string(&self) -> String {
        self.to_string()
    }
}

impl Field for &StorePath {
    fn write(self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(STORE_DIR.as_bytes());
        out.push(b'/');
        out.extend_from_slice(self.base_name().as_bytes());
        events::quote_in_place(out, start);
    }
}

/// Mirrors store-dir-config.cc:makeStorePath(type, const Hash &, name). The
/// `<algo>:<base16>` of the hash goes straight into the payload.
pub fn make_store_path(ty: &str, hash: &Hash, name: &str) -> Result<StorePath, StorePathError> {
    let mut s = String::with_capacity(ty.len() + STORE_DIR.len() + name.len() + 80);
    s.push_str(ty);
    s.push(':');
    hash.write_string_with(&mut s, HashFormat::Base16, true);
    s.push(':');
    s.push_str(STORE_DIR);
    s.push(':');
    s.push_str(name);
    from_payload(&s, name)
}

fn from_payload(payload: &str, name: &str) -> Result<StorePath, StorePathError> {
    let h = compress_hash(&hash_bytes(HashAlgo::Sha256, payload.as_bytes()), 20);
    StorePath::new(&h, name)
}

/// Mirrors derivations.cc:outputPathName: "out" keeps the name, others get `<name>-<id>`.
pub fn output_path_name(drv_name: &str, output_name: &str) -> String {
    if output_name == "out" {
        drv_name.to_string()
    } else {
        format!("{drv_name}-{output_name}")
    }
}

/// Mirrors store-dir-config.cc:makeOutputPath.
pub fn make_output_path(
    output_name: &str,
    hash: &Hash,
    drv_name: &str,
) -> Result<StorePath, StorePathError> {
    make_store_path(
        &format!("output:{output_name}"),
        hash,
        &output_path_name(drv_name, output_name),
    )
}

/// Mirrors content-address.hh:FileIngestionMethod (Git is not supported).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IngestionMethod {
    Flat,
    Recursive,
    /// `outputHashMode = "text"`: the output is addressed as `.drv` files and
    /// `builtins.toFile` are. A derivation whose output is itself a `.drv`
    /// needs it, so it is the producer half of a dynamic derivation.
    Text,
}

impl IngestionMethod {
    /// Mirrors content-address.cc:ContentAddressMethod::renderPrefix.
    pub fn prefix(self) -> &'static str {
        match self {
            IngestionMethod::Flat => "",
            IngestionMethod::Recursive => "r:",
            IngestionMethod::Text => "text:",
        }
    }

    /// Mirrors primops.cc:derivationStrict `handleHashMode` ("flat", "recursive", "nar", "text").
    pub fn parse(s: &str) -> Option<IngestionMethod> {
        match s {
            "flat" => Some(IngestionMethod::Flat),
            "recursive" | "nar" => Some(IngestionMethod::Recursive),
            "text" => Some(IngestionMethod::Text),
            _ => None,
        }
    }
}

/// Mirrors store-dir-config.cc:makeType: references are appended to the type string.
fn make_type(mut ty: String, references: &BTreeSet<StorePath>, self_ref: bool) -> String {
    let n: usize = references
        .iter()
        .map(|r| r.base_name().len() + STORE_DIR.len() + 2)
        .sum();
    ty.reserve(n + 5);
    for r in references {
        ty.push(':');
        ty.push_str(STORE_DIR);
        ty.push('/');
        ty.push_str(r.base_name());
    }
    if self_ref {
        ty.push_str(":self");
    }
    ty
}

/// Mirrors store-dir-config.cc:makeFixedOutputPath. Only recursive sha256 may carry references.
pub fn make_fixed_output_path(
    method: IngestionMethod,
    hash: &Hash,
    name: &str,
    references: &BTreeSet<StorePath>,
) -> Result<StorePath, StorePathError> {
    if method == IngestionMethod::Text {
        if hash.algo != HashAlgo::Sha256 {
            return Err(StorePathError(format!(
                "text content address of '{name}' must be sha256, not {}",
                hash.algo.name()
            )));
        }
        return make_text_path(name, hash, references);
    }
    if hash.algo == HashAlgo::Sha256 && method == IngestionMethod::Recursive {
        return make_store_path(&make_type("source".into(), references, false), hash, name);
    }
    if !references.is_empty() {
        return Err(StorePathError(format!(
            "fixed output derivation '{name}' is not allowed to refer to other store paths"
        )));
    }
    let mut payload = String::with_capacity(96);
    payload.push_str("fixed:out:");
    payload.push_str(method.prefix());
    hash.write_string_with(&mut payload, HashFormat::Base16, true);
    payload.push(':');
    let digest = hash_bytes(HashAlgo::Sha256, payload.as_bytes());
    make_store_path("output:out", &digest, name)
}

/// Mirrors store-dir-config.cc:makeFixedOutputPathFromCA for TextInfo (builtins.toFile, .drv files).
pub fn make_text_path(
    name: &str,
    hash: &Hash,
    references: &BTreeSet<StorePath>,
) -> Result<StorePath, StorePathError> {
    assert_eq!(hash.algo, HashAlgo::Sha256);
    make_store_path(&make_type("text".into(), references, false), hash, name)
}

/// Mirrors derivations.cc:hashPlaceholder: the env value of a floating CA output.
pub fn hash_placeholder(output_name: &str) -> String {
    let h = hash_bytes(
        HashAlgo::Sha256,
        format!("nix-output:{output_name}").as_bytes(),
    );
    format!("/{}", h.to_nix32())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::nar::tests::fixture_tree;
    use crate::store::nar::{nar_hash, nar_hash_filtered};

    fn set(paths: &[&str]) -> BTreeSet<StorePath> {
        paths.iter().map(|p| StorePath::parse(p).unwrap()).collect()
    }

    // nix-instantiate --eval -E 'builtins.toFile "foo" "bar"'
    #[test]
    fn to_file() {
        let h = hash_bytes(HashAlgo::Sha256, b"bar");
        let p = make_text_path("foo", &h, &BTreeSet::new()).unwrap();
        assert_eq!(
            p.to_string(),
            "/nix/store/vxjiwkjkn7x4079qvh1jkl5pn05j2aw0-foo"
        );
        assert_eq!(p.name(), "foo");
        assert_eq!(p.hash_part(), "vxjiwkjkn7x4079qvh1jkl5pn05j2aw0");
    }

    // nix-instantiate --eval -E 'let a = builtins.toFile "a" "x"; in builtins.toFile "b" "ref ${a}"'
    #[test]
    fn to_file_with_reference() {
        let a = make_text_path("a", &hash_bytes(HashAlgo::Sha256, b"x"), &BTreeSet::new()).unwrap();
        assert_eq!(
            a.to_string(),
            "/nix/store/12wigjpizrn8axaqxj288q1b751qmwya-a"
        );
        let contents = format!("ref {a}");
        let refs: BTreeSet<StorePath> = [a].into_iter().collect();
        let b = make_text_path(
            "b",
            &hash_bytes(HashAlgo::Sha256, contents.as_bytes()),
            &refs,
        )
        .unwrap();
        assert_eq!(
            b.to_string(),
            "/nix/store/kbnsdqvza4r1hzcr5vs2bhx8kv150g37-b"
        );
    }

    // nix-instantiate --eval -E '"${./tree}"' (tree from nar.rs fixture_tree)
    #[test]
    fn path_copy_directory() {
        let tree = fixture_tree("pathcopy");
        let h = nar_hash(&tree).unwrap();
        let p = make_fixed_output_path(IngestionMethod::Recursive, &h, "tree", &BTreeSet::new())
            .unwrap();
        assert_eq!(
            p.to_string(),
            "/nix/store/7achjqw6z8fr92f66lxll1lr3yzpqvc3-tree"
        );
    }

    // nix-instantiate --eval -E '"${./tree/a.txt}"'
    #[test]
    fn path_copy_file() {
        let tree = fixture_tree("pathfile");
        let h = nar_hash(&tree.join("a.txt")).unwrap();
        let p = make_fixed_output_path(IngestionMethod::Recursive, &h, "a.txt", &BTreeSet::new())
            .unwrap();
        assert_eq!(
            p.to_string(),
            "/nix/store/zwki0nhs1pipypg3dgza58q0vyaa78pl-a.txt"
        );
    }

    // nix-instantiate --eval -E '"${builtins.filterSource (p: t: baseNameOf p != "sub") ./tree}"'
    #[test]
    fn filter_source() {
        let tree = fixture_tree("filtersrc");
        let (h, _) = nar_hash_filtered(&tree, HashAlgo::Sha256, &mut |p, _| {
            p.file_name().unwrap() != "sub"
        })
        .unwrap();
        let p = make_fixed_output_path(IngestionMethod::Recursive, &h, "tree", &BTreeSet::new())
            .unwrap();
        assert_eq!(
            p.to_string(),
            "/nix/store/1rmqvsn4swz7n7vkjyvj6cldipi3gp8b-tree"
        );
    }

    // nix-instantiate --eval -E '"${builtins.path { path = ./tree; name = "mytree"; }}"'
    #[test]
    fn path_with_name() {
        let tree = fixture_tree("pathname");
        let h = nar_hash(&tree).unwrap();
        let p = make_fixed_output_path(IngestionMethod::Recursive, &h, "mytree", &BTreeSet::new())
            .unwrap();
        assert_eq!(
            p.to_string(),
            "/nix/store/ymkhbrxgqx2ii22w4kqj3jjfmcq5smzk-mytree"
        );
    }

    // nix-instantiate --eval --strict -E 'let d = builtins.derivation { name = "fx"; system = "x86_64-linux";
    //   builder = "/bin/sh"; outputHash = "ba78...15ad"; outputHashAlgo = "sha256"; }; in d.outPath'
    #[test]
    fn fixed_flat_sha256() {
        let h = Hash::parse_any(
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            Some(HashAlgo::Sha256),
        )
        .unwrap();
        let p = make_fixed_output_path(IngestionMethod::Flat, &h, "fx", &BTreeSet::new()).unwrap();
        assert_eq!(
            p.to_string(),
            "/nix/store/rfn4vzwhmb8r4k8gp16g27dqwjji5s37-fx"
        );
    }

    // same as above with outputHashMode = "recursive" and name "fr"
    #[test]
    fn fixed_recursive_sha256() {
        let h = Hash::parse_any(
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            Some(HashAlgo::Sha256),
        )
        .unwrap();
        let p =
            make_fixed_output_path(IngestionMethod::Recursive, &h, "fr", &BTreeSet::new()).unwrap();
        assert_eq!(
            p.to_string(),
            "/nix/store/s05rl80bpix1186wb5f471k7fxwwp7lf-fr"
        );
    }

    // same with outputHash = "a9993e364706816aba3e25717850c26c9cd0d89d"; outputHashAlgo = "sha1"; name "fs1"
    #[test]
    fn fixed_flat_sha1() {
        let h = Hash::parse_any(
            "a9993e364706816aba3e25717850c26c9cd0d89d",
            Some(HashAlgo::Sha1),
        )
        .unwrap();
        let p = make_fixed_output_path(IngestionMethod::Flat, &h, "fs1", &BTreeSet::new()).unwrap();
        assert_eq!(
            p.to_string(),
            "/nix/store/nawmi4lnsz5x8kk5pf3j4afk5smhklqr-fs1"
        );
        assert!(
            make_fixed_output_path(
                IngestionMethod::Flat,
                &h,
                "fs1",
                &set(&["/nix/store/12wigjpizrn8axaqxj288q1b751qmwya-a"])
            )
            .is_err()
        );
    }

    #[test]
    fn parse_and_accessors() {
        let p = StorePath::parse("/nix/store/vxjiwkjkn7x4079qvh1jkl5pn05j2aw0-foo.drv").unwrap();
        assert!(p.is_derivation());
        assert_eq!(p.name(), "foo.drv");
        assert_eq!(p.base_name(), "vxjiwkjkn7x4079qvh1jkl5pn05j2aw0-foo.drv");
        assert!(StorePath::parse("/nix/store/vxjiwkjkn7x4079qvh1jkl5pn05j2aw0-foo/bar").is_err());
        assert!(StorePath::parse("/nix/store/vxjiwkjkn7x4079qvh1jkl5pn05j2aw0foo").is_err());
        assert!(StorePath::parse("/nix/store/vxjiwkjkn7x4079qvh1jkl5pn05j2awe-foo").is_err());
        assert!(StorePath::parse("/tmp/vxjiwkjkn7x4079qvh1jkl5pn05j2aw0-foo").is_err());
    }

    #[test]
    fn check_name_rules() {
        assert!(check_name("hello-2.12").is_ok());
        assert!(check_name("a+b_c?d=e.f").is_ok());
        assert!(check_name(".hidden").is_ok());
        assert!(check_name("..x").is_ok());
        assert!(check_name("").is_err());
        assert!(check_name(".").is_err());
        assert!(check_name("..").is_err());
        assert!(check_name(".-x").is_err());
        assert!(check_name("..-x").is_err());
        assert!(check_name("a b").is_err());
        assert!(check_name("a/b").is_err());
        assert!(check_name(&"a".repeat(212)).is_err());
        assert!(check_name(&"a".repeat(211)).is_ok());
    }

    #[test]
    fn output_names() {
        assert_eq!(output_path_name("hello", "out"), "hello");
        assert_eq!(output_path_name("hello", "dev"), "hello-dev");
    }

    // nix-instantiate --eval -E 'builtins.placeholder "out"'
    #[test]
    fn placeholder() {
        assert_eq!(
            hash_placeholder("out"),
            "/1rz4g4znpzjwh1xymhjpm42vipw92pr73vdgl6xs1hycac8kf2n9"
        );
    }
}
