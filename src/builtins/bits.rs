//! One bit per element of a builtin's input, for what `filter` and
//! `removeAttrs` keep or drop: on the stack up to 1024 elements, so the
//! result is allocated once at its size.

use smallvec::{SmallVec, smallvec};

pub struct Bits(SmallVec<[u64; 16]>);

impl Bits {
    pub fn new(n: usize) -> Bits {
        if n <= 1024 {
            Bits(SmallVec::from_buf([0; 16]))
        } else {
            Bits(smallvec![0; n.div_ceil(64)])
        }
    }

    #[inline]
    pub fn get(&self, i: usize) -> bool {
        self.0[i / 64] >> (i % 64) & 1 != 0
    }

    /// Sets bit `i`; answers whether it was clear.
    #[inline]
    pub fn set(&mut self, i: usize) -> bool {
        let (word, bit) = (&mut self.0[i / 64], 1u64 << (i % 64));
        let was_clear = *word & bit == 0;
        *word |= bit;
        was_clear
    }
}

#[cfg(test)]
mod tests {
    use super::Bits;

    #[test]
    fn small_and_big() {
        for n in [1, 64, 1024, 1025, 5000] {
            let mut b = Bits::new(n);
            for i in (0..n).step_by(3) {
                assert!(b.set(i));
                assert!(!b.set(i));
            }
            for i in 0..n {
                assert_eq!(b.get(i), i % 3 == 0, "n={n} i={i}");
            }
        }
    }
}
