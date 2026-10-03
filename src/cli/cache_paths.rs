//! `iets cache paths`: the printing over `api::watch::watch_paths`.

/// NUL-separated paths on stdout, or `--json` with the kind of each.
pub fn cmd_cache_paths(file: &str, json: bool) -> i32 {
    let w = match crate::api::watch::watch_paths(std::path::Path::new(file)) {
        Ok(w) => w,
        Err(e) => return e.report(),
    };
    if w.len() == crate::api::watch::MAX {
        crate::log::line(&format!(
            "iets: the answer may depend on more than {0} paths; the first {0} are printed",
            crate::api::watch::MAX
        ));
    }
    if json {
        let a: Vec<serde_json::Value> = w
            .iter()
            .map(|x| serde_json::json!({ "path": x.path, "kind": x.kind }))
            .collect();
        println!("{}", serde_json::Value::Array(a));
    } else {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        for x in &w {
            let _ = out.write_all(x.path.as_bytes());
            let _ = out.write_all(b"\0");
        }
    }
    0
}
