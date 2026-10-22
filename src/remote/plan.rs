//! The upload plan: the closure of the root derivations, and the sources the daemon lacks
//! hashed the way Nix addresses them.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::Hash as Hashable;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::{fs, io, thread};

use crate::builtins::derivation::DrvInfo;
use crate::error::{Error, Result};
use crate::session;
use crate::store::drv::Derivation;
use crate::store::hash::{self, Hash, HashAlgo, Hasher};
use crate::store::nar::dump_string;
use crate::store::path::{IngestionMethod, StorePath, make_fixed_output_path, store_dir};
use crate::store::source::{SourceKind, SourceOrigin, SrcPath};

/// One object the daemon must hold before it can build the root.
#[derive(Clone, Debug)]
pub enum Object {
    /// A source tree plus the NAR hash Nix addresses it by.
    Source {
        path: StorePath,
        at: SrcPath,
        kind: SourceKind,
        nar_hash: Hash,
        nar_size: u64,
    },
    /// A derivation: its ATerm. `references` is inputSrcs plus inputDrvs, what the worker
    /// protocol needs to register the `.drv` as a text path. `hashes` is filled by
    /// `prehash_drvs` before the upload starts, so the connection only writes bytes.
    Drv {
        path: StorePath,
        aterm: Arc<Vec<u8>>,
        references: BTreeSet<StorePath>,
        hashes: OnceLock<DrvHashes>,
    },
}

/// What registering a `.drv` needs beyond its bytes: the flat sha256 of the ATerm, which is
/// its `text:sha256:` content address, and the hash and size of its NAR.
#[derive(Clone, Debug)]
pub struct DrvHashes {
    pub text: Hash,
    pub nar_hash: Hash,
    pub nar_size: u64,
}

impl DrvHashes {
    pub fn of(aterm: &[u8]) -> DrvHashes {
        let mut nar = Hasher::new(HashAlgo::Sha256);
        dump_string(aterm, &mut nar).expect("a hasher never fails");
        DrvHashes {
            text: hash::hash_bytes(HashAlgo::Sha256, aterm),
            nar_size: nar.bytes_written(),
            nar_hash: nar.finish(),
        }
    }
}

/// Hashes the ATerm of every `.drv` in `objects` on a thread pool. Without this each ATerm is
/// hashed twice inside the framed upload, one drv at a time, with the daemon waiting.
pub fn prehash_drvs(objects: &[&Object]) {
    let todo: Vec<(&OnceLock<DrvHashes>, &[u8])> = objects
        .iter()
        .filter_map(|o| match o {
            Object::Drv { aterm, hashes, .. } if hashes.get().is_none() => {
                Some((hashes, aterm.as_slice()))
            }
            _ => None,
        })
        .collect();
    if todo.len() < 2 {
        for (cell, aterm) in todo {
            let _ = cell.set(DrvHashes::of(aterm));
        }
        return;
    }
    let threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(todo.len());
    let next = AtomicUsize::new(0);
    thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(session::bind(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= todo.len() {
                        break;
                    }
                    let (cell, aterm) = todo[i];
                    let _ = cell.set(DrvHashes::of(aterm));
                }
            }));
        }
    });
}

impl Object {
    /// A source object, hashing the tree only when evaluation did not already report the
    /// hash under the same filter.
    pub fn source(path: StorePath, o: SourceOrigin) -> io::Result<Object> {
        let (nar_hash, nar_size) = match o.nar {
            Some(h) => h,
            None => hash_source(&o.at)?,
        };
        Ok(Object::Source {
            path,
            at: o.at,
            kind: o.kind,
            nar_hash,
            nar_size,
        })
    }

    pub fn drv(path: StorePath, aterm: Arc<Vec<u8>>, references: BTreeSet<StorePath>) -> Object {
        Object::Drv {
            path,
            aterm,
            references,
            hashes: OnceLock::new(),
        }
    }

    pub fn store_path(&self) -> &StorePath {
        match self {
            Object::Source { path, .. } | Object::Drv { path, .. } => path,
        }
    }
}

/// The closure of the roots: what the daemon must hold before it can build them. Sources are
/// only named here; `resolve` locates and hashes the ones the daemon lacks.
#[derive(Debug, Default)]
pub struct Plan {
    /// Input sources in registration order: a `toFile` text after the sources it references.
    pub sources: Vec<StorePath>,
    /// The eval-time origin of every source in `sources` that this evaluation added.
    pub origins: HashMap<StorePath, SourceOrigin>,
    /// Derivations in post-order: each after its input drvs.
    pub drvs: Vec<Object>,
    /// Input drvs that were not created by this evaluation and were read from `/nix/store`.
    pub store_drvs: usize,
}

/// The missing sources after `Plan::resolve`: what the client uploads and what it leaves to
/// the daemon.
#[derive(Debug, Default)]
pub struct Resolved {
    /// Hashed sources, in the order they were asked for.
    pub objects: Vec<Object>,
    /// Sources the client cannot produce: not in the local store, or not content-addressed
    /// the way v1 sends (an input-addressed build output pinned with `builtins.storePath`).
    /// The daemon substitutes them.
    pub substitute: Vec<StorePath>,
    /// Sources with no eval-time origin, read from the local store and hashed.
    pub store_sources: usize,
}

/// Levels of the graph `missing_top_down` asks about one call at a time
/// before it asks about the closure of what is left in one call.
const TOP_DOWN_LEVELS: usize = 2;

/// The nodes reachable from `roots` through nodes lacking any of their
/// `keys`. A store holds the closure of every path it holds, so nothing below
/// a held node is asked about; a warm store answers in one or two small calls.
/// `answered`: what the store holds of the roots' keys, if already asked.
pub fn missing_top_down<N: Copy + Eq + Hashable>(
    roots: Vec<N>,
    keys: &dyn Fn(N) -> Result<Vec<StorePath>>,
    children: &dyn Fn(N) -> Vec<N>,
    answered: Option<&HashSet<String>>,
    query: &mut dyn FnMut(&[StorePath]) -> Result<HashSet<String>>,
) -> Result<HashSet<N>> {
    let mut valid: HashSet<String> = answered.cloned().unwrap_or_default();
    let mut asked: HashSet<N> = HashSet::new();
    if answered.is_some() {
        asked.extend(&roots);
    }
    let mut seen: HashSet<N> = HashSet::new();
    let mut missing: HashSet<N> = HashSet::new();
    let mut level = roots;
    for depth in 0.. {
        level.retain(|n| seen.insert(*n));
        if level.is_empty() {
            break;
        }
        let mut ask: Vec<N> = level.iter().copied().filter(|n| asked.insert(*n)).collect();
        if depth >= TOP_DOWN_LEVELS {
            let mut stack = level.clone();
            while let Some(n) = stack.pop() {
                for c in children(n) {
                    if asked.insert(c) {
                        ask.push(c);
                        stack.push(c);
                    }
                }
            }
        }
        if !ask.is_empty() {
            let mut paths = Vec::new();
            for n in ask {
                paths.extend(keys(n)?);
            }
            valid.extend(query(&paths)?);
        }
        let mut next = Vec::new();
        for n in level {
            if keys(n)?.iter().any(|k| !valid.contains(k.base_name())) {
                missing.insert(n);
                next.extend(children(n));
            }
        }
        level = next;
    }
    Ok(missing)
}

impl Plan {
    /// The plan's paths in the closure of `roots` that the store lacks
    /// (`missing_top_down`). A valid drv answers for all it references.
    pub fn query_missing(
        &self,
        roots: &[StorePath],
        answered: Option<&HashSet<String>>,
        query: &mut dyn FnMut(&[StorePath]) -> Result<HashSet<String>>,
    ) -> Result<HashSet<&StorePath>> {
        let planned: HashSet<&StorePath> = self
            .sources
            .iter()
            .chain(self.drvs.iter().map(Object::store_path))
            .collect();
        let refs: HashMap<&StorePath, Vec<&StorePath>> = self
            .drvs
            .iter()
            .filter_map(|o| match o {
                Object::Drv {
                    path, references, ..
                } => Some((
                    path,
                    references
                        .iter()
                        .filter_map(|r| planned.get(r).copied())
                        .collect(),
                )),
                Object::Source { .. } => None,
            })
            .collect();
        let roots = roots
            .iter()
            .filter_map(|r| planned.get(r).copied())
            .collect();
        missing_top_down(
            roots,
            &|p| Ok(vec![p.clone()]),
            &|p| refs.get(p).cloned().unwrap_or_default(),
            answered,
            query,
        )
    }

    /// Locates and hashes `missing`, a subset of `sources` in plan order: eval-time origins
    /// first, the rest through `locate` in one batch, hashed on a thread pool.
    pub fn resolve(&self, missing: &[StorePath], locate: &Locator<'_>) -> Result<Resolved> {
        let mut infos: Vec<Option<SourceOrigin>> = Vec::with_capacity(missing.len());
        let mut unknown: Vec<StorePath> = Vec::new();
        for p in missing {
            let info = self.origins.get(p).cloned();
            if info.is_none() {
                unknown.push(p.clone());
            }
            infos.push(info);
        }
        let mut substitute = Vec::new();
        if !unknown.is_empty() {
            let found = locate(&unknown)?;
            if found.len() != unknown.len() {
                return Err(Error::Plan(
                    "locator returned the wrong number of sources".into(),
                ));
            }
            let mut found = found.into_iter();
            for (p, slot) in missing.iter().zip(infos.iter_mut()) {
                if slot.is_some() {
                    continue;
                }
                match found.next().expect("one answer per unknown source") {
                    Some(info) => *slot = Some(info),
                    None => substitute.push(p.clone()),
                }
            }
        }
        let (paths, infos): (Vec<&StorePath>, Vec<SourceOrigin>) = missing
            .iter()
            .zip(infos)
            .filter_map(|(p, i)| i.map(|i| (p, i)))
            .unzip();
        let store_sources = unknown.len() - substitute.len();
        Ok(Resolved {
            objects: hash_sources(&paths, &infos)?,
            substitute,
            store_sources,
        })
    }
}

/// Checks that a planned source still hashes to its store path: the NAR of the original tree
/// must yield the path eval computed, or the daemon rejects it. Only sources the plan hashed
/// itself need this. A tree whose hash comes from evaluation cannot fail it by construction;
/// if such a tree changed on disk after evaluation, the daemon catches it, because
/// `AddMultipleToStore` recomputes the NAR hash of a `fixed:r` path and rejects a mismatch,
/// and `write_object` refuses to send a NAR whose length is not the recorded `nar_size`.
fn check_source_path(path: &StorePath, kind: &SourceKind, nar_hash: &Hash) -> Result<()> {
    if let SourceKind::Source = kind {
        let expected = make_fixed_output_path(
            IngestionMethod::Recursive,
            nar_hash,
            path.name(),
            &BTreeSet::new(),
        )?;
        if &expected != path {
            return Err(Error::Plan(format!(
                "source '{path}' changed since evaluation (now {expected})"
            )));
        }
    }
    Ok(())
}

/// Maps input source store paths that eval did not record, in one batch, to where to read
/// them and how they were addressed; `None` when the local side cannot produce the path and
/// the daemon must substitute it. `iets build` asks the local store.
pub type Locator<'a> = dyn Fn(&[StorePath]) -> Result<Vec<Option<SourceOrigin>>> + 'a;

/// What the evaluation knows: the drv table and the origin of every added source. Both may
/// be empty, in which case everything is read from `/nix/store`.
#[derive(Clone, Copy)]
pub struct Inputs<'a> {
    pub drvs: &'a HashMap<StorePath, DrvInfo>,
    pub sources: &'a HashMap<StorePath, SourceOrigin>,
}

/// The NAR hash and size of a source, one pass over the bytes.
pub fn hash_source(at: &SrcPath) -> io::Result<(Hash, u64)> {
    let mut nar = Hasher::new(HashAlgo::Sha256);
    at.nar(&mut nar)?;
    let size = nar.bytes_written();
    Ok((nar.finish(), size))
}

/// One derivation of a walk: its ATerm and what it references.
pub type Node = (StorePath, Arc<Vec<u8>>, BTreeSet<StorePath>);

/// A drv's ATerm and references: this evaluation's (`drv`), else
/// `/nix/store`'s. The flag is true for the second.
pub fn load_drv(
    p: &StorePath,
    drv: &dyn Fn(&StorePath) -> Option<DrvInfo>,
) -> Result<(Arc<Vec<u8>>, BTreeSet<StorePath>, bool)> {
    if let Some(info) = drv(p)
        && let Some(aterm) = info.aterm
    {
        return Ok((aterm, info.references, false));
    }
    let full = format!("{}/{}", store_dir(), p.base_name());
    let text =
        fs::read(&full).map_err(|e| Error::io(format!("cannot read derivation '{full}'"), e))?;
    let drv = Derivation::parse(&text, Derivation::name_from_path(p)?)?;
    let mut references = drv.input_srcs;
    references.extend(drv.input_drvs.into_keys());
    Ok((Arc::new(text), references, true))
}

/// The drvs `roots` need, each after every drv it references, and the
/// sources they name, each after the sources its `toFile` text references
/// (`text_refs`): the daemon registers a path only once its references are
/// valid, and a text can point at a copied path no derivation lists. The walk
/// stops at `known`. The count is the drvs read from `/nix/store`.
pub fn walk(
    roots: &[StorePath],
    known: &HashSet<StorePath>,
    drv: &dyn Fn(&StorePath) -> Option<DrvInfo>,
    text_refs: &dyn Fn(&StorePath) -> Option<BTreeSet<StorePath>>,
) -> Result<(Vec<Node>, Vec<StorePath>, usize)> {
    fn source(
        p: StorePath,
        known: &HashSet<StorePath>,
        text_refs: &dyn Fn(&StorePath) -> Option<BTreeSet<StorePath>>,
        seen: &mut HashSet<StorePath>,
        out: &mut Vec<StorePath>,
    ) {
        if known.contains(&p) || !seen.insert(p.clone()) {
            return;
        }
        for r in text_refs(&p).unwrap_or_default() {
            if !r.is_derivation() {
                source(r, known, text_refs, seen, out);
            }
        }
        out.push(p);
    }
    let mut seen: HashSet<StorePath> = HashSet::new();
    let mut drvs: Vec<Node> = Vec::new();
    let mut srcs: Vec<StorePath> = Vec::new();
    let mut from_store = 0;
    type Pending = Option<(Arc<Vec<u8>>, BTreeSet<StorePath>)>;
    let mut stack: Vec<(StorePath, Pending)> =
        roots.iter().rev().map(|p| (p.clone(), None)).collect();
    while let Some((p, done)) = stack.pop() {
        if let Some((aterm, refs)) = done {
            drvs.push((p, aterm, refs));
            continue;
        }
        if known.contains(&p) || !seen.insert(p.clone()) {
            continue;
        }
        let (aterm, refs, stored) = load_drv(&p, drv)?;
        from_store += usize::from(stored);
        stack.push((p, Some((aterm, refs.clone()))));
        for r in refs {
            if r.is_derivation() {
                if !known.contains(&r) && !seen.contains(&r) {
                    stack.push((r, None));
                }
            } else {
                source(r, known, text_refs, &mut seen, &mut srcs);
            }
        }
    }
    Ok((drvs, srcs, from_store))
}

/// Computes the closure of `roots`: input drvs recursively and every input source, without
/// reading or hashing a source. Every drv comes from `inputs`; what is not there is read from
/// `/nix/store` and counted in the plan. Fixed-output drvs are ordinary drvs here; the builder
/// fetches them.
pub fn plan(roots: &[StorePath], inputs: Inputs<'_>) -> Result<Plan> {
    let (drvs, sources, store_drvs) = walk(
        roots,
        &HashSet::new(),
        &|p| inputs.drvs.get(p).cloned(),
        &|p| inputs.sources.get(p)?.text_refs().cloned(),
    )?;
    let origins = sources
        .iter()
        .filter_map(|p| inputs.sources.get(p).map(|o| (p.clone(), o.clone())))
        .collect();
    Ok(Plan {
        sources,
        origins,
        drvs: drvs
            .into_iter()
            .map(|(p, aterm, refs)| Object::drv(p, aterm, refs))
            .collect(),
        store_drvs,
    })
}

/// Dumps and hashes every source on a thread pool; results keep the input order.
fn hash_sources(sources: &[&StorePath], infos: &[SourceOrigin]) -> Result<Vec<Object>> {
    let threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(sources.len().max(1));
    let next = Mutex::new(0usize);
    let results: Vec<Mutex<Option<Result<Object>>>> =
        sources.iter().map(|_| Mutex::new(None)).collect();
    thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(session::bind(|| {
                loop {
                    let i = {
                        let mut n = next.lock().unwrap();
                        let i = *n;
                        *n += 1;
                        i
                    };
                    if i >= sources.len() {
                        break;
                    }
                    let path = sources[i];
                    let info = &infos[i];
                    let from_eval = info.nar.is_some();
                    let r = Object::source(path.clone(), info.clone())
                        .map_err(|e| Error::io(format!("cannot dump '{path}'"), e))
                        .and_then(|o| {
                            if let Object::Source { nar_hash, kind, .. } = &o
                                && !from_eval
                            {
                                check_source_path(path, kind, nar_hash)?;
                            }
                            Ok(o)
                        });
                    *results[i].lock().unwrap() = Some(r);
                }
            }));
        }
    });
    results
        .into_iter()
        .map(|m| m.into_inner().unwrap().expect("every source hashed"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::nar::tests::{Tree, tree};
    use crate::store::nar::{nar_hash, nar_size};
    use rstest::rstest;

    #[rstest]
    fn hash_source_matches_nar_hash(tree: Tree) {
        let (h, size) = hash_source(&SrcPath::fs(tree.clone(), None)).unwrap();
        assert_eq!(h, nar_hash(&tree).unwrap());
        assert_eq!(size, nar_size(&tree).unwrap());
    }

    fn sp(name: &str) -> StorePath {
        StorePath::parse(&format!("/nix/store/{:0>32}-{name}", name.len())).unwrap()
    }

    fn node(name: &str, refs: &[&StorePath]) -> Object {
        Object::Drv {
            path: sp(name),
            aterm: Arc::new(Vec::new()),
            references: refs.iter().map(|&r| r.clone()).collect(),
            hashes: OnceLock::new(),
        }
    }

    /// root -> a -> b -> src_b, root -> src
    fn chain() -> Plan {
        let (src, src_b) = (sp("src"), sp("src-b"));
        let b = node("b.drv", &[&src_b]);
        let a = node("a.drv", &[b.store_path()]);
        let root = node("root.drv", &[a.store_path(), &src]);
        Plan {
            sources: vec![src_b, src],
            origins: HashMap::new(),
            drvs: vec![b, a, root],
            store_drvs: 0,
        }
    }

    /// The calls made and the names found missing, both sorted per call.
    fn calls(
        plan: &Plan,
        held: &[&str],
        answered: Option<&HashSet<String>>,
    ) -> (Vec<Vec<String>>, Vec<String>) {
        let held: HashSet<String> = held.iter().map(|n| sp(n).base_name().to_string()).collect();
        let mut seen = Vec::new();
        let missing = plan
            .query_missing(&[sp("root.drv")], answered, &mut |ps| {
                let mut names: Vec<String> = ps.iter().map(|p| p.name().to_string()).collect();
                names.sort();
                seen.push(names);
                Ok(ps
                    .iter()
                    .map(|p| p.base_name().to_string())
                    .filter(|b| held.contains(b))
                    .collect())
            })
            .unwrap();
        let mut missing: Vec<String> = missing.iter().map(|p| p.name().to_string()).collect();
        missing.sort();
        (seen, missing)
    }

    #[test]
    fn a_held_root_answers_for_its_closure() {
        let (seen, missing) = calls(&chain(), &["root.drv"], None);
        assert_eq!(seen, [["root.drv"]]);
        assert!(missing.is_empty());
    }

    #[test]
    fn a_held_input_answers_for_its_closure() {
        let (seen, missing) = calls(&chain(), &["a.drv", "src"], None);
        assert_eq!(seen, vec![vec!["root.drv"], vec!["a.drv", "src"]]);
        assert_eq!(missing, ["root.drv"]);
    }

    #[test]
    fn a_cold_store_is_asked_the_rest_in_one_call() {
        let (seen, missing) = calls(&chain(), &[], None);
        assert_eq!(
            seen,
            vec![
                vec!["root.drv"],
                vec!["a.drv", "src"],
                vec!["b.drv", "src-b"]
            ]
        );
        assert_eq!(missing, ["a.drv", "b.drv", "root.drv", "src", "src-b"]);
    }

    #[test]
    fn an_answered_root_is_not_asked_again() {
        let (seen, missing) = calls(&chain(), &["a.drv", "src"], Some(&HashSet::new()));
        assert_eq!(seen, [["a.drv", "src"]]);
        assert_eq!(missing, ["root.drv"]);
    }

    #[test]
    fn memory_source_matches_dump_string() {
        let o = SourceOrigin::text(b"hello\n".to_vec(), BTreeSet::new());
        let (h, size) = hash_source(&o.at).unwrap();
        assert_eq!(h, crate::store::nar::nar_hash_of_string(b"hello\n"));
        assert!(size > 0);
    }
}
