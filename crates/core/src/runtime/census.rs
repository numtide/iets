//! Memory census behind the `stats2` cargo feature and `IETS_STATS=2`:
//! histograms by object kind, duplicate strings and contexts, `//` shapes,
//! and which objects were never read. Every hook is `cfg(feature)` so the
//! default build carries none of it (docs/explanation/evaluation.md).

use std::array;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use super::value::{
    AttrsObj, Ctx, EnvObj, Kind, ListObj, MAX_LEAVES, StringObj, UNION_MIN, leaf_count,
};
use super::word::Word;
use crate::syntax::symbol::fx_bytes;
use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};

pub const SIZE_BUCKETS: [&str; 11] = [
    "0", "1", "2", "3-4", "5-8", "9-16", "17-32", "33-64", "65-128", "129-256", "257+",
];

pub fn size_bucket(n: usize) -> usize {
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

/// How `update_attrs` built a result, in its order of tests.
pub const UPD_PATHS: [&str; 5] = [
    "empty side",
    "flat copy",
    "union",
    "flatten a, union",
    "flat, over leaves",
];

/// Uses of one `//` result by the end of the run.
pub const UPD_USES: [&str; 7] = [
    "never used",
    "once: select",
    "once: iterated",
    "once: left of //",
    "once: right of //",
    "2-3 uses",
    "4+ uses",
];

#[derive(Default)]
pub struct Upd {
    pub n: u64,
    pub paths: [u64; 5],
    pub a: [u64; 11],
    pub b: [u64; 11],
    pub out: [u64; 11],
    /// Right-hand keys the left side already has, and the ones it adds.
    pub over: [u64; 11],
    pub add: [u64; 11],
    /// Overwritten share of the right side: none, under half, half or
    /// more, all.
    pub over_share: [u64; 4],
    pub sum_a: u64,
    pub sum_b: u64,
    pub sum_out: u64,
    /// Arena bytes the update allocated: copies, flattens, union headers.
    pub bytes: u64,
    /// Updates whose right operand is itself a `//` expression: the
    /// inner result is built only to be merged again.
    pub nested_right: u64,
    /// Length of the left-nested `//` chain each result ends.
    pub chain: [u64; 11],
    /// (a bucket, b bucket) counts.
    pub shape: [[u64; 11]; 11],
    /// Each new result: chain length, uses as left and as right operand.
    pub results: HashMap<*const AttrsObj, (u32, u32, u32)>,
    /// By the file of the `//`: path, count, bytes.
    pub by_file: HashMap<usize, (&'static [u8], u64, u64)>,
}

/// The branch `update_attrs` takes; read before the update, which may
/// cache a flat copy of `a`.
pub fn upd_path(a: &'static AttrsObj, b: &'static AttrsObj) -> usize {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    if a.kind == Kind::Attrs && a.len() < UNION_MIN {
        return 1;
    }
    let (la, lb) = (leaf_count(a), leaf_count(b));
    if la + lb <= MAX_LEAVES as usize {
        2
    } else if lb < MAX_LEAVES as usize {
        3
    } else {
        4
    }
}

/// Lookups on a flat set, on a union through its flat copy, on a union
/// over its leaves that hit, the leaves all union lookups probed, and the
/// union lookups that missed. `get` has no arena at hand, hence statics;
/// a census run has one worker.
pub static GETS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

#[inline]
pub fn note_get(kind: usize, probes: u64) {
    GETS[kind].fetch_add(1, Ordering::Relaxed);
    if probes != 0 {
        GETS[3].fetch_add(probes, Ordering::Relaxed);
    }
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
    pub fn scope_enter(&mut self, bytes: usize) -> bool {
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
    pub fn scope_leave(&mut self, bytes: usize) -> (u64, u64) {
        self.scope_open = false;
        ((bytes - self.scope_start) as u64, self.scope_escapes)
    }

    #[inline]
    pub fn note_thunk_created(&mut self, w: u64) {
        if self.scope_open {
            self.scope_thunks.insert(w);
        }
    }

    #[inline]
    pub fn note_thunk_forced(&mut self, w: u64) {
        if self.scope_open && !self.scope_thunks.contains(&w) {
            self.scope_escapes += 1;
        }
    }

    pub fn note_env(&mut self, e: *const EnvObj, n: usize) {
        let b = size_bucket(n);
        self.env_hist[b].0 += 1;
        self.env_hist[b].1 += (8 + 8 * n) as u64;
        self.envs.push((e, n as u32));
    }

    pub fn note_attrs(&mut self, a: *const AttrsObj, origin: u8, n: usize) {
        self.attrs_hist[origin as usize][size_bucket(n)] += 1;
        self.attrs.push(a);
    }

    pub fn note_list(&mut self, l: *const ListObj) {
        self.lists.push(l);
    }

    pub fn note_string(&mut self, s: *const StringObj, bytes: &[u8], ctx_bytes: usize) {
        let b = size_bucket(bytes.len());
        self.str_len_hist[b].0 += 1;
        self.str_len_hist[b].1 += bytes.len() as u64;
        if !self.str_seen.insert(fx_bytes(bytes)) {
            self.str_dups += 1;
            self.str_dup_bytes += bytes.len() as u64;
        }
        if ctx_bytes > 0 {
            let n = ctx_bytes / size_of::<Ctx>();
            let cb = size_bucket(n);
            self.ctx_len_hist[cb].0 += 1;
            self.ctx_len_hist[cb].1 += ctx_bytes as u64;
        }
        self.strings.push(s);
    }

    pub fn note_ctx(&mut self, ctx: &[Ctx]) {
        if ctx.is_empty() {
            return;
        }
        let mut key: Vec<u8> = Vec::new();
        for c in ctx {
            match c {
                Ctx::Opaque(p) => {
                    key.push(1);
                    key.extend_from_slice(p);
                }
                Ctx::DrvDeep(p) => {
                    key.push(2);
                    key.extend_from_slice(p);
                }
                Ctx::Built { drv, output } => {
                    key.push(3);
                    key.extend_from_slice(drv);
                    key.push(0);
                    key.extend_from_slice(output);
                }
                Ctx::Impure(p) => {
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

    /// One `a // b` that gave `out`; `bytes` is what it allocated and
    /// `file` the source file of the operator.
    pub fn note_upd(
        &mut self,
        path: usize,
        a: &'static AttrsObj,
        b: &'static AttrsObj,
        out: Word,
        bytes: u64,
        file: Option<(usize, &'static [u8])>,
    ) {
        let u = &mut self.upd;
        let (la, lb) = (a.len_unmarked(), b.len_unmarked());
        // SAFETY: `//` only ever returns a set.
        let o = unsafe { &*out.ptr::<AttrsObj>() };
        let lo = o.len_unmarked();
        let over = la + lb - lo;
        u.n += 1;
        u.paths[path] += 1;
        u.a[size_bucket(la)] += 1;
        u.b[size_bucket(lb)] += 1;
        u.out[size_bucket(lo)] += 1;
        u.over[size_bucket(over)] += 1;
        u.add[size_bucket(lb - over)] += 1;
        u.over_share[match over {
            0 => 0,
            _ if over == lb => 3,
            _ if 2 * over < lb => 1,
            _ => 2,
        }] += 1;
        u.sum_a += la as u64;
        u.sum_b += lb as u64;
        u.sum_out += lo as u64;
        u.bytes += bytes;
        u.shape[size_bucket(la)][size_bucket(lb)] += 1;
        let mut chain = 1;
        if let Some(r) = u.results.get_mut(&(a as *const AttrsObj)) {
            r.1 += 1;
            chain += r.0;
        }
        if let Some(r) = u.results.get_mut(&(b as *const AttrsObj)) {
            r.2 += 1;
        }
        if path != 0 {
            u.chain[size_bucket(chain as usize)] += 1;
            u.results.insert(o, (chain, 0, 0));
        }
        if let Some((key, name)) = file {
            let f = u.by_file.entry(key).or_insert((name, 0, 0));
            f.1 += 1;
            f.2 += bytes;
        }
    }

    pub fn report(&self, out: &mut String) {
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
            "update (//): n={} allocated={} bytes; right operand a // expression: {}; mean a={:.1} b={:.1} out={:.1}",
            u.n,
            u.bytes,
            u.nested_right,
            u.sum_a as f64 / u.n.max(1) as f64,
            u.sum_b as f64 / u.n.max(1) as f64,
            u.sum_out as f64 / u.n.max(1) as f64
        );
        let _ = write!(out, "update paths:");
        for (name, c) in UPD_PATHS.iter().zip(u.paths) {
            let _ = write!(out, " {name}={c}");
        }
        let _ = writeln!(out);
        let _ = writeln!(out, "update sizes (counts):");
        let _ = write!(out, "  {:>12}", "");
        for b in SIZE_BUCKETS {
            let _ = write!(out, " {b:>8}");
        }
        let _ = writeln!(out);
        for (name, row) in [
            ("left", &u.a),
            ("right", &u.b),
            ("result", &u.out),
            ("overwritten", &u.over),
            ("added", &u.add),
            ("chain", &u.chain),
        ] {
            let _ = write!(out, "  {name:>12}");
            for c in row {
                let _ = write!(out, " {c:>8}");
            }
            let _ = writeln!(out);
        }
        let [none, under, half, all] = u.over_share;
        let _ = writeln!(
            out,
            "update right side overwritten: none={none} under half={under} half or more={half} all={all}"
        );
        let mut uses = [0u64; 7];
        for (r, (_, left, right)) in &u.results {
            // SAFETY: results are arena sets, alive until exit.
            let flags = unsafe { (**r).flags };
            let sel = (flags & 0x7f) as u32;
            let iter = (flags >> 7) as u32;
            uses[match (sel + iter + left + right, sel, iter, left) {
                (0, ..) => 0,
                (1, 1, ..) => 1,
                (1, _, 1, _) => 2,
                (1, .., 1) => 3,
                (1, ..) => 4,
                (2..=3, ..) => 5,
                _ => 6,
            }] += 1;
        }
        let _ = write!(out, "update results by use ({}):", u.results.len());
        for (name, c) in UPD_USES.iter().zip(uses) {
            let _ = write!(out, " {name}={c}");
        }
        let _ = writeln!(out);
        let g: [u64; 5] = array::from_fn(|i| GETS[i].load(Ordering::Relaxed));
        let _ = writeln!(
            out,
            "attrset lookups: flat={} union via flat copy={} union over leaves: hit={} miss={} (leaves probed {})",
            g[0], g[1], g[2], g[4], g[3]
        );
        let mut files: Vec<_> = u.by_file.values().collect();
        files.sort_by_key(|f| Reverse(f.1));
        let _ = writeln!(out, "update by file of the // (count, bytes):");
        for (name, n, bytes) in files.iter().take(20) {
            let _ = writeln!(
                out,
                "  {n:>9} {bytes:>11} {}",
                String::from_utf8_lossy(name)
            );
        }
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
        files.sort_by_key(|f| Reverse(f.1));
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
