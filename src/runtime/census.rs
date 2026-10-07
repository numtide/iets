//! Memory census behind the `stats2` cargo feature and `IETS_STATS=2`:
//! histograms by object kind, duplicate strings and contexts, `//` shapes,
//! and which objects were never read. Every hook is `cfg(feature)` so the
//! default build carries none of it (docs/explanation/evaluation.md).

use std::collections::HashSet;

use super::value::{AttrsObj, EnvObj, ListObj, StringObj};
use crate::syntax::symbol::fx_bytes;
use std::fmt::Write;

pub const SIZE_BUCKETS: [&str; 11] = [
    "0", "1", "2", "3-4", "5-8", "9-16", "17-32", "33-64", "65-128", "129-256", "257+",
];

pub(crate) fn size_bucket(n: usize) -> usize {
    match n {
        0 => 0,
        1 => 1,
        2 => 2,
        3..=4 => 3,
        5..=8 => 4,
        9..=16 => 5,
        17..=32 => 6,
        33..=64 => 7,
        65..=128 => 8,
        129..=256 => 9,
        _ => 10,
    }
}

/// Producers of attrsets, stored in the object header byte.
pub const ORIGINS: [&str; 10] = [
    "other",
    "lit",
    "upd",
    "mapAttrs",
    "listToAttrs",
    "removeAttrs",
    "zipAttrsWith",
    "intersect",
    "groupBy",
    "pos",
];

#[derive(Default)]
pub struct Upd {
    pub n: u64,
    pub sum_a: u64,
    pub sum_b: u64,
    pub sum_out: u64,
    /// Results more than four times the size of the right operand.
    pub big_a: u64,
    /// Bytes spent copying entries of the left operand.
    pub a_bytes: u64,
    pub a_was_upd: u64,
    pub b_was_upd: u64,
    /// (a bucket, b bucket) counts.
    pub shape: [[u64; 11]; 11],
}

#[derive(Default)]
pub struct Census {
    pub env_hist: [(u64, u64); 11],
    pub attrs_hist: [[u64; 11]; 10],
    pub str_len_hist: [(u64, u64); 11],
    pub str_seen: HashSet<u64>,
    pub str_dups: u64,
    pub str_dup_bytes: u64,
    pub ctx_seen: HashSet<u64>,
    pub ctx_dups: u64,
    pub ctx_dup_bytes: u64,
    pub ctx_len_hist: [(u64, u64); 11],
    pub upd: Upd,
    pub forced: [u64; 4],
    pub envs: Vec<(*const EnvObj, u32)>,
    pub attrs: Vec<*const AttrsObj>,
    pub strings: Vec<*const StringObj>,
    pub lists: Vec<*const ListObj>,
    pub lambda_closed: u64,
    pub get_attr_pos_calls: u64,
    pub ir_by_file: Vec<(u32, u64)>,
    /// Scoped-release assessment: thunks created inside the open scope,
    /// and forces of older thunks inside it (each one writes a value
    /// built inside the scope into memory that outlives it).
    pub scope_open: bool,
    pub scope_thunks: HashSet<u64>,
    pub scope_escapes: u64,
    pub scope_start: usize,
    /// `tryEval` scopes: calls, failed, bytes inside failed, failed with
    /// no escape and their bytes.
    pub try_scopes: [u64; 5],
    /// eval-jobs job units: units, bytes, units with no escape, their bytes.
    pub job_scopes: [u64; 4],
}

/// `bucket=count` for every bucket that is not empty.
fn by_bucket(counts: &[u64]) -> String {
    counts
        .iter()
        .enumerate()
        .filter(|(_, c)| **c > 0)
        .map(|(i, c)| format!("{}={}", SIZE_BUCKETS[i], c))
        .collect::<Vec<_>>()
        .join(" ")
}

impl Census {
    /// Open an escape-tracking scope at `bytes` allocated; false when one
    /// is already open (nested scopes are not tracked).
    pub(crate) fn scope_enter(&mut self, bytes: usize) -> bool {
        if self.scope_open {
            return false;
        }
        self.scope_open = true;
        self.scope_thunks.clear();
        self.scope_escapes = 0;
        self.scope_start = bytes;
        true
    }

    /// Close the scope: (bytes allocated inside, escapes).
    pub(crate) fn scope_leave(&mut self, bytes: usize) -> (u64, u64) {
        self.scope_open = false;
        ((bytes - self.scope_start) as u64, self.scope_escapes)
    }

    #[inline]
    pub(crate) fn note_thunk_created(&mut self, w: u64) {
        if self.scope_open {
            self.scope_thunks.insert(w);
        }
    }

    #[inline]
    pub(crate) fn note_thunk_forced(&mut self, w: u64) {
        if self.scope_open && !self.scope_thunks.contains(&w) {
            self.scope_escapes += 1;
        }
    }

    pub(crate) fn note_env(&mut self, e: *const EnvObj, n: usize) {
        let b = size_bucket(n);
        self.env_hist[b].0 += 1;
        self.env_hist[b].1 += (8 + 8 * n) as u64;
        self.envs.push((e, n as u32));
    }

    pub(crate) fn note_attrs(&mut self, a: *const AttrsObj, origin: u8, n: usize) {
        self.attrs_hist[origin as usize][size_bucket(n)] += 1;
        self.attrs.push(a);
    }

    pub(crate) fn note_list(&mut self, l: *const ListObj) {
        self.lists.push(l);
    }

    pub(crate) fn note_string(&mut self, s: *const StringObj, bytes: &[u8], ctx_bytes: usize) {
        let b = size_bucket(bytes.len());
        self.str_len_hist[b].0 += 1;
        self.str_len_hist[b].1 += bytes.len() as u64;
        if !self.str_seen.insert(fx_bytes(bytes)) {
            self.str_dups += 1;
            self.str_dup_bytes += bytes.len() as u64;
        }
        if ctx_bytes > 0 {
            let n = ctx_bytes / size_of::<super::value::Ctx>();
            let cb = size_bucket(n);
            self.ctx_len_hist[cb].0 += 1;
            self.ctx_len_hist[cb].1 += ctx_bytes as u64;
        }
        self.strings.push(s);
    }

    pub(crate) fn note_ctx(&mut self, ctx: &[super::value::Ctx]) {
        if ctx.is_empty() {
            return;
        }
        let mut key: Vec<u8> = Vec::new();
        for c in ctx {
            match c {
                super::value::Ctx::Opaque(p) => {
                    key.push(1);
                    key.extend_from_slice(p);
                }
                super::value::Ctx::DrvDeep(p) => {
                    key.push(2);
                    key.extend_from_slice(p);
                }
                super::value::Ctx::Built { drv, output } => {
                    key.push(3);
                    key.extend_from_slice(drv);
                    key.push(0);
                    key.extend_from_slice(output);
                }
                super::value::Ctx::Impure(p) => {
                    key.push(4);
                    key.extend_from_slice(p.bytes());
                }
            }
            key.push(0);
        }
        if !self.ctx_seen.insert(fx_bytes(&key)) {
            self.ctx_dups += 1;
            self.ctx_dup_bytes += size_of_val(ctx) as u64;
        }
    }

    pub(crate) fn note_upd(&mut self, a: &AttrsObj, b: &AttrsObj) {
        let u = &mut self.upd;
        let out = a.len() + b.len();
        u.n += 1;
        u.sum_a += a.len() as u64;
        u.sum_b += b.len() as u64;
        u.sum_out += out as u64;
        if out > 4 * b.len() {
            u.big_a += 1;
        }
        u.a_bytes += (a.len() * 16) as u64;
        if a.origin == 2 {
            u.a_was_upd += 1;
        }
        if b.origin == 2 {
            u.b_was_upd += 1;
        }
        u.shape[size_bucket(a.len())][size_bucket(b.len())] += 1;
    }

    pub(crate) fn report(&self, out: &mut String) {
        let _ = writeln!(out, "--- census (IETS_STATS=2) ---");
        let _ = writeln!(out, "envs by slot count (count, bytes):");
        for (i, (c, b)) in self.env_hist.iter().enumerate() {
            if *c > 0 {
                let _ = writeln!(out, "  {:>8} {:>10} {:>12}", SIZE_BUCKETS[i], c, b);
            }
        }
        // SAFETY: every pointer was allocated in the arena, which lives
        // until exit; the read-bit lives in the low bit of `up`.
        let (mut env_unread, mut env_unread_bytes) = (0u64, 0u64);
        let mut env_unread_by: [u64; 11] = [0; 11];
        let mut env_uncaptured_by: [u64; 11] = [0; 11];
        let mut env_uncaptured_bytes = 0u64;
        for (e, n) in &self.envs {
            let up = unsafe { (**e).up } as usize;
            if up & 1 == 0 {
                env_unread += 1;
                env_unread_bytes += (8 + 8 * *n) as u64;
                env_unread_by[size_bucket(*n as usize)] += 1;
            }
            if up & 2 == 0 {
                env_uncaptured_by[size_bucket(*n as usize)] += 1;
                env_uncaptured_bytes += (8 + 8 * *n) as u64;
            }
        }
        let _ = writeln!(
            out,
            "envs never captured by a thunk, closure or child env ({} bytes); by slots: {}",
            env_uncaptured_bytes,
            by_bucket(&env_uncaptured_by)
        );
        let _ = writeln!(
            out,
            "envs never read: {} of {} ({} bytes); by slots: {}",
            env_unread,
            self.envs.len(),
            env_unread_bytes,
            by_bucket(&env_unread_by)
        );
        let _ = writeln!(out, "attrsets by producer and size (counts):");
        let _ = write!(out, "  {:>12}", "");
        for b in SIZE_BUCKETS {
            let _ = write!(out, " {b:>8}");
        }
        let _ = writeln!(out, " {:>9}", "total");
        for (o, row) in self.attrs_hist.iter().enumerate() {
            let total: u64 = row.iter().sum();
            if total == 0 {
                continue;
            }
            let _ = write!(out, "  {:>12}", ORIGINS[o]);
            for c in row {
                let _ = write!(out, " {c:>8}");
            }
            let _ = writeln!(out, " {total:>9}");
        }
        let mut never: [u64; 10] = [0; 10];
        let mut never_bytes: [u64; 10] = [0; 10];
        let mut once: [u64; 10] = [0; 10];
        let mut iterated: [u64; 10] = [0; 10];
        let mut total_bytes: [u64; 10] = [0; 10];
        for a in &self.attrs {
            let a = unsafe { &**a };
            let o = a.origin as usize;
            let bytes = (8 + 16 * a.len()) as u64;
            total_bytes[o] += bytes;
            let sel = a.flags & 0x7f;
            if a.flags & 0x80 != 0 {
                iterated[o] += 1;
            }
            if sel == 0 && a.flags & 0x80 == 0 {
                never[o] += 1;
                never_bytes[o] += bytes;
            } else if sel == 1 && a.flags & 0x80 == 0 {
                once[o] += 1;
            }
        }
        let _ = writeln!(
            out,
            "attrsets use: producer: total bytes / never selected nor iterated (bytes) / selected once only / iterated"
        );
        for o in 0..10 {
            if total_bytes[o] == 0 {
                continue;
            }
            let _ = writeln!(
                out,
                "  {:>12}: {:>11} / {:>8} ({:>10}) / {:>8} / {:>8}",
                ORIGINS[o], total_bytes[o], never[o], never_bytes[o], once[o], iterated[o]
            );
        }
        let u = &self.upd;
        let _ = writeln!(
            out,
            "update (//): n={} avg a={:.1} b={:.1} out={:.1}; out>4*b: {}; bytes copying a: {}; a was upd: {}; b was upd: {}",
            u.n,
            u.sum_a as f64 / u.n.max(1) as f64,
            u.sum_b as f64 / u.n.max(1) as f64,
            u.sum_out as f64 / u.n.max(1) as f64,
            u.big_a,
            u.a_bytes,
            u.a_was_upd,
            u.b_was_upd
        );
        let _ = writeln!(out, "update shape (rows: a size, cols: b size):");
        let _ = write!(out, "  {:>8}", "");
        for b in SIZE_BUCKETS {
            let _ = write!(out, " {b:>8}");
        }
        let _ = writeln!(out);
        for (i, row) in u.shape.iter().enumerate() {
            if row.iter().sum::<u64>() == 0 {
                continue;
            }
            let _ = write!(out, "  {:>8}", SIZE_BUCKETS[i]);
            for c in row {
                let _ = write!(out, " {c:>8}");
            }
            let _ = writeln!(out);
        }
        let _ = writeln!(out, "strings by length (count, bytes):");
        for (i, (c, b)) in self.str_len_hist.iter().enumerate() {
            if *c > 0 {
                let _ = writeln!(out, "  {:>8} {:>10} {:>12}", SIZE_BUCKETS[i], c, b);
            }
        }
        let mut str_unread = 0u64;
        for s in &self.strings {
            if unsafe { (**s).flags } == 0 {
                str_unread += 1;
            }
        }
        let _ = writeln!(
            out,
            "strings: duplicates by content: {} ({} bytes); never viewed as a string: {} of {}",
            self.str_dups,
            self.str_dup_bytes,
            str_unread,
            self.strings.len()
        );
        let _ = writeln!(out, "contexts by element count (count, bytes):");
        for (i, (c, b)) in self.ctx_len_hist.iter().enumerate() {
            if *c > 0 {
                let _ = writeln!(out, "  {:>8} {:>10} {:>12}", SIZE_BUCKETS[i], c, b);
            }
        }
        let _ = writeln!(
            out,
            "contexts: duplicates by content: {} ({} bytes)",
            self.ctx_dups, self.ctx_dup_bytes
        );
        let _ = writeln!(
            out,
            "thunks forced: thunk={} thunk_app={} thunk_app2={} thunk_map={}",
            self.forced[0], self.forced[1], self.forced[2], self.forced[3]
        );
        let mut list_unread = 0u64;
        let mut list_unread_bytes = 0u64;
        for l in &self.lists {
            let l = unsafe { &**l };
            if l.flags == 0 {
                list_unread += 1;
                list_unread_bytes += (8 + 8 * l.len as usize) as u64;
            }
        }
        let _ = writeln!(
            out,
            "lists never read: {} of {} ({} bytes)",
            list_unread,
            self.lists.len(),
            list_unread_bytes
        );
        let _ = writeln!(
            out,
            "closures of closed lambdas (no captured variables): {}",
            self.lambda_closed
        );
        let _ = writeln!(out, "unsafeGetAttrPos calls: {}", self.get_attr_pos_calls);
        let t = &self.try_scopes;
        let _ = writeln!(
            out,
            "tryEval scopes (outermost): calls={} failed={} bytes inside failed={} failed without escape={} ({} bytes)",
            t[0], t[1], t[2], t[3], t[4]
        );
        let j = &self.job_scopes;
        let _ = writeln!(
            out,
            "eval-jobs units: {} ({} bytes inside); without escape: {} ({} bytes)",
            j[0], j[1], j[2], j[3]
        );
        let mut files: Vec<(u32, u64)> = self.ir_by_file.clone();
        files.sort_by_key(|f| std::cmp::Reverse(f.1));
        let total: u64 = files.iter().map(|f| f.1).sum();
        let _ = writeln!(
            out,
            "ir bytes: total {} over {} files; top files:",
            total,
            files.len()
        );
        for (f, b) in files.iter().take(12) {
            let _ = writeln!(out, "  file{f} {b}");
        }
    }
}
