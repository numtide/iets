/// Interned identifier or attribute name. Ids are dense and start at 0.
/// Attrsets sort by id, so id order is arbitrary but stable within a run.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Sym(pub u32);

use std::collections::{HashMap, HashSet};

pub use super::interner::Interner;
use std::hash::{BuildHasherDefault, Hasher};

/// FxHash: one multiply per word. SipHash was 4% of the profile on the
/// interner and the compiler's scope maps, whose keys are short.
#[derive(Default, Clone, Copy)]
pub struct FxHasher(u64);

const FX_SEED: u64 = 0x517c_c1b7_2722_0a95;

impl FxHasher {
    #[inline]
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(FX_SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut b = bytes;
        while b.len() >= 8 {
            self.add(u64::from_le_bytes(b[..8].try_into().unwrap()));
            b = &b[8..];
        }
        if b.len() >= 4 {
            self.add(u32::from_le_bytes(b[..4].try_into().unwrap()) as u64);
            b = &b[4..];
        }
        for &x in b {
            self.add(x as u64);
        }
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

/// Content hash of a byte string, its length mixed in.
#[inline]
pub fn fx_bytes(bytes: &[u8]) -> u64 {
    let mut h = FxHasher::default();
    // Four independent chains: one chain waits on a multiply per 8 bytes,
    // and the dedup table hashes kilobyte builder scripts.
    let (blocks, rest) = bytes.as_chunks::<32>();
    if !blocks.is_empty() {
        let mut l = [FxHasher::default(); 4];
        for b in blocks {
            for (h, w) in l.iter_mut().zip(b.as_chunks::<8>().0) {
                h.add(u64::from_le_bytes(*w));
            }
        }
        for x in l {
            h.add(x.0);
        }
    }
    h.write(rest);
    h.write_usize(bytes.len());
    h.finish()
}

pub type FxBuild = BuildHasherDefault<FxHasher>;
pub type FxHashMap<K, V> = HashMap<K, V, FxBuild>;
pub type FxHashSet<K> = HashSet<K, FxBuild>;

#[cfg(test)]
mod tests {
    use super::fx_bytes;
    use std::collections::HashSet;

    #[test]
    fn fx_bytes_sees_every_byte_and_its_place() {
        // a byte changed at any place, including the four lanes and the
        // tail, or words swapped between lanes, gives a new hash
        let base: Vec<u8> = (0..100u8).collect();
        let mut seen = HashSet::new();
        assert!(seen.insert(fx_bytes(&base)));
        for i in 0..base.len() {
            let mut s = base.clone();
            s[i] ^= 0x80;
            assert!(seen.insert(fx_bytes(&s)), "byte {i}");
        }
        for (a, b) in [(0, 8), (8, 24), (0, 32)] {
            let mut s = base.clone();
            for k in 0..8 {
                s.swap(a + k, b + k);
            }
            assert!(seen.insert(fx_bytes(&s)), "swap {a} {b}");
        }
        assert!(seen.insert(fx_bytes(&base[..99])));
    }
}
