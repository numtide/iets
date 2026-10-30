//! One build dispatch. Every command that realises a derivation goes through
//! [`Dispatch`]: the builder URL, the upload plan, the connection, the upload
//! and the build are one object, and a root's output path is one lookup
//! (docs/explanation/plumbing-and-porcelain.md).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, io};

use crate::build::executor::Places;
use crate::build::upload::PlanUpload;
use crate::build::{Dispatcher, Outcome, Target};
use crate::builtins::derivation::DrvInfo;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::eval::Evaluator;
use crate::locate::locate_in_store;
use crate::provenance::Provenance;
use crate::remote::plan::{self, Inputs};
use crate::remote::worker::Missing;
use crate::settings;
use crate::srcpath::SourceOrigin;
use crate::store::path::StorePath;
use crate::syntax::symbol::FxHashMap;

// What `Dispatch::open` takes, so a caller above this line can name it all
// without reaching past the api (tests/layering.rs).
pub use crate::build::render::Style;
pub use crate::remote::plan::Plan;
pub use crate::remote::worker::{Options, Transport};
use crate::store::drv::Derivation;

/// One derivation and one of its outputs a command asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    pub drv: StorePath,
    pub output: String,
}

impl Root {
    pub fn paths(roots: &[Root]) -> Vec<StorePath> {
        roots.iter().map(|r| r.drv.clone()).collect()
    }
}

/// The Nix settings the native commands read: they take no `--option`, and an
/// unreadable nix.conf leaves the defaults.
pub fn config() -> Config {
    Config::load(&[]).unwrap_or_else(|_| Config::empty())
}

/// Every place the builds may go: the flag, else the `builder` setting, else
/// nix.conf's `store` (docs/reference/settings.md).
pub fn places(url: Option<&str>, cfg: &Config) -> Result<Places> {
    Places::parse(&settings::builders(url, cfg), &cfg.system())
}

/// The one store a command addresses, for the commands that address a store
/// rather than build: `plumbing missing`, `prefetch`, `--dry-run`. With
/// several places that is the first one.
pub fn transport(url: Option<&str>, cfg: &Config) -> Result<Transport> {
    Ok(places(url, cfg)?.get(0).transport.clone())
}

/// The upload plan for `roots`, from the derivations and sources this
/// evaluation made.
pub fn plan(ev: &Evaluator, roots: &[StorePath]) -> Result<Plan> {
    let drvs = ev.drv.drvs.lock();
    let sources = ev.known.origins();
    plan_with(roots, &drvs, &sources)
}

/// [`plan()`] over a derivation table the caller holds: a cache hit, a rewritten
/// fixed-output derivation, or nothing at all, in which case every input drv
/// is read from `/nix/store`.
pub fn plan_with(
    roots: &[StorePath],
    drvs: &FxHashMap<StorePath, DrvInfo>,
    sources: &HashMap<StorePath, SourceOrigin>,
) -> Result<Plan> {
    plan::plan(roots, Inputs { drvs, sources })
}

/// One derivation read back from its ATerm text, as the table [`plan_with`]
/// takes: a `.drv` file, or an entry of a `--dump-drvs` directory.
pub fn drv_info(path: &StorePath, text: Vec<u8>) -> Result<DrvInfo> {
    let drv = Derivation::parse(&text, Derivation::name_from_path(path)?)?;
    let mut references = drv.input_srcs;
    references.extend(drv.input_drvs.into_keys());
    Ok(DrvInfo {
        outputs: drv.outputs.into_keys().collect(),
        references,
        aterm: Some(Arc::new(text)),
        pos: None,
        attr: None,
    })
}

/// The derivations `--dump-drvs DIR` wrote: an evaluation's whole ATerm table,
/// which is what lets one process plan or build a closure another evaluated and
/// never wrote to the store (docs/reference/plumbing.md). Sources are not in it, so they
/// still come from the local store.
pub fn drvs_in_dir(dir: &Path) -> Result<FxHashMap<StorePath, DrvInfo>> {
    let at = |e: io::Error| Error::io(format!("reading '{}'", dir.display()), e);
    let mut out = FxHashMap::default();
    for entry in fs::read_dir(dir).map_err(at)? {
        let entry = entry.map_err(at)?;
        let base = entry.file_name().to_string_lossy().into_owned();
        let Ok(path) = StorePath::from_base_name(&base) else {
            continue;
        };
        if !path.is_derivation() {
            continue;
        }
        let text = fs::read(entry.path()).map_err(|e| Error::io(format!("reading '{base}'"), e))?;
        let info = drv_info(&path, text)?;
        out.insert(path, info);
    }
    Ok(out)
}

/// The plan's locator with no `--source` overrides: for a command that drives
/// the upload on a connection of its own because it goes on to speak the
/// worker protocol itself (`nix-shell`, `print-dev-env`).
pub fn locate_store(paths: &[StorePath]) -> Result<Vec<Option<SourceOrigin>>> {
    locate_in_store(paths, &HashMap::new())
}

/// A planned closure, the connection that carries it, and the builds.
pub struct Dispatch {
    plan: Plan,
    /// `--source STOREPATH=LOCAL`: read those paths from a local tree.
    overrides: HashMap<String, PathBuf>,
    pub d: Dispatcher,
}

impl Dispatch {
    pub fn open(
        transport: &Transport,
        options: &Options,
        plan: Plan,
        prov: Provenance,
        style: Style,
    ) -> Result<Dispatch> {
        Ok(Dispatch::with(
            Dispatcher::open(transport, options, prov, style)?,
            plan,
        ))
    }

    /// A dispatcher the caller made itself, for a command that owns its
    /// reporter.
    pub fn with(d: Dispatcher, plan: Plan) -> Dispatch {
        Dispatch {
            plan,
            overrides: HashMap::new(),
            d,
        }
    }

    pub fn overrides(mut self, sources: HashMap<String, PathBuf>) -> Dispatch {
        self.overrides = sources;
        self
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Uploads what `batch` needs and builds it in one op.
    pub fn submit(&mut self, batch: Vec<Target>) -> Result<Vec<Outcome>> {
        let locate = |paths: &[StorePath]| locate_in_store(paths, &self.overrides);
        let mut up = PlanUpload::new(&self.plan, &locate);
        self.d.submit(&mut up, batch)
    }

    /// `--no-build`: register the closure and stop.
    pub fn upload_only(&mut self, roots: &[StorePath]) -> Result<()> {
        let locate = |paths: &[StorePath]| locate_in_store(paths, &self.overrides);
        let mut up = PlanUpload::new(&self.plan, &locate);
        self.d.upload_only(&mut up, roots)
    }

    /// `--dry-run`: what building `targets` would build and fetch.
    pub fn missing(&mut self, targets: &[Target]) -> Result<Missing> {
        let locate = |paths: &[StorePath]| locate_in_store(paths, &self.overrides);
        let mut up = PlanUpload::new(&self.plan, &locate);
        self.d.missing(&mut up, targets)
    }

    /// Builds every root in one op. Roots that share a derivation become one
    /// target carrying both outputs; `name` says what a derivation is called.
    pub fn realise(
        &mut self,
        roots: &[Root],
        name: &dyn Fn(&StorePath) -> String,
    ) -> Result<Built> {
        let mut batch: Vec<Target> = Vec::new();
        for r in roots {
            match batch.iter_mut().find(|t| t.drv == r.drv) {
                Some(t) if t.outputs.contains(&r.output) => {}
                Some(t) => t.outputs.push(r.output.clone()),
                None => batch.push(Target::new(
                    &name(&r.drv),
                    r.drv.clone(),
                    vec![r.output.clone()],
                )),
            }
        }
        Ok(Built(
            self.submit(batch)?
                .into_iter()
                .map(|o| (o.drv.to_string(), o.outputs))
                .collect(),
        ))
    }
}

/// What a build put in the store, by derivation and output name.
pub struct Built(HashMap<String, BTreeMap<String, String>>);

impl Built {
    /// One output path per root, in the order the roots were given.
    pub fn paths(&self, roots: &[Root]) -> Result<Vec<String>> {
        roots
            .iter()
            .map(|r| {
                let drv = r.drv.to_string();
                (self.0.get(&drv).and_then(|m| m.get(&r.output)).cloned()).ok_or_else(|| {
                    Error::Store(format!("derivation '{drv}' has no output '{}'", r.output))
                })
            })
            .collect()
    }
}
