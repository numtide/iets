//! `iets cache gc`: sweep both cache stores by entry age.

use std::time::Duration;

use crate::cli::help::{arg_error, unknown_flag, value};
use crate::{cachefs, error};

const DAY: u64 = 24 * 60 * 60;

/// `iets cache gc [--ttl DAYS] [--dry-run] [--cache-dir DIR]`. Entries unread
/// for longer than the TTL go; a swept tracing entry simply misses next time.
pub fn cmd_cache_gc(argv: &[String]) -> i32 {
    let mut ttl_days = cachefs::TTL_DAYS;
    let mut dry_run = false;
    let mut cache_dir: Option<String> = None;
    let mut i = 0;
    while i < argv.len() {
        let s = argv[i].as_str();
        match s {
            "--dry-run" | "-n" => dry_run = true,
            "--ttl" => {
                i += 1;
                let v = value(argv, i, s);
                // a TTL whose seconds do not fit made the window wrap and
                // selected every fresh entry
                ttl_days = v
                    .parse::<u64>()
                    .ok()
                    .filter(|d| d.checked_mul(DAY).is_some())
                    .unwrap_or_else(|| {
                        arg_error(format!("--ttl wants a number of days, got '{v}'"))
                    });
            }
            "--cache-dir" => {
                i += 1;
                cache_dir = Some(value(argv, i, s).clone());
            }
            s if s.starts_with('-') => unknown_flag(s),
            s => arg_error(format!("unexpected argument '{s}'")),
        }
        i += 1;
    }
    let stores = cachefs::stores(cache_dir.as_deref());
    if stores.is_empty() {
        return error::Error::Cache("no cache directory (no HOME and no --cache-dir)".into())
            .report();
    }
    let ttl = Duration::from_secs(ttl_days * DAY);
    for (root, shape) in stores {
        let s = cachefs::sweep(&root, shape, ttl, dry_run);
        println!(
            "{}: {} of {} entries, {:.1} MiB{}",
            root.display(),
            s.removed,
            s.scanned,
            s.bytes as f64 / (1024.0 * 1024.0),
            if dry_run { " (dry run)" } else { " removed" }
        );
    }
    0
}
