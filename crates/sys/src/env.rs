//! The environment a command reads, handed in by the host.

use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct Env(Option<Arc<HashMap<String, OsString>>>);

impl Env {
    pub fn process() -> Env {
        Env(None)
    }

    pub fn from_pairs<K: Into<String>, V: Into<OsString>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Env {
        Env(Some(Arc::new(
            pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )))
    }

    /// Every Nix configuration path points at /nonexistent.
    pub fn isolated() -> Env {
        Env::from_pairs([
            ("NIX_CONF_DIR", "/nonexistent"),
            ("NIX_USER_CONF_FILES", ""),
            ("XDG_CONFIG_HOME", "/nonexistent"),
            ("XDG_CONFIG_DIRS", "/nonexistent"),
        ])
    }

    pub fn with<K: Into<String>, V: Into<OsString>>(
        &self,
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Env {
        let mut m = self.0.as_deref().cloned().unwrap_or_default();
        m.extend(pairs.into_iter().map(|(k, v)| (k.into(), v.into())));
        Env(Some(Arc::new(m)))
    }

    pub fn var(&self, name: &str) -> Option<String> {
        self.var_os(name)?.into_string().ok()
    }

    pub fn var_os(&self, name: &str) -> Option<OsString> {
        match &self.0 {
            None => env::var_os(name),
            Some(m) => m.get(name).cloned(),
        }
    }

    pub fn non_empty(&self, name: &str) -> Option<String> {
        self.var(name).filter(|s| !s.is_empty())
    }
}
