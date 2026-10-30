//! Positions of the bytes a string printer has to escape, 16 at a time.
//!
//! Builder scripts have a newline every ~20 bytes, so the scan walks
//! every hit of a block from one mask instead of rescanning after each.

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{
    __m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_min_epu8, _mm_movemask_epi8, _mm_or_si128,
    _mm_set1_epi8,
};

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::{
    vceqq_u8, vcltq_u8, vdupq_n_u8, vget_lane_u64, vld1q_u8, vorrq_u8, vreinterpret_u64_u8,
    vreinterpretq_u16_u8, vshrn_n_u16,
};

/// Mask bits per byte: `movemask` gives one, NEON's narrowing shift four.
#[cfg(target_arch = "aarch64")]
const BITS: u32 = 4;
#[cfg(not(target_arch = "aarch64"))]
const BITS: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Escapes {
    /// `"`, `\`, `\t`, `\n`, `\r` (derivations.cc:printString)
    Aterm,
    /// `"`, `\` and every byte below 0x20
    Json,
}

impl Escapes {
    #[cfg(any(test, not(any(target_arch = "x86_64", target_arch = "aarch64"))))]
    fn hit(self, b: u8) -> bool {
        match self {
            Escapes::Aterm => matches!(b, b'"' | b'\\' | b'\t' | b'\n' | b'\r'),
            Escapes::Json => b < 0x20 || b == b'"' || b == b'\\',
        }
    }

    #[cfg(target_arch = "x86_64")]
    fn mask(self, block: &[u8; 16]) -> u64 {
        // SAFETY: SSE2 is part of x86_64; the load is unaligned and reads
        // exactly the 16 bytes of `block`.
        unsafe {
            let x = _mm_loadu_si128(block.as_ptr().cast::<__m128i>());
            let eq = |c: u8| _mm_cmpeq_epi8(x, _mm_set1_epi8(c as i8));
            let quote = _mm_or_si128(eq(b'"'), eq(b'\\'));
            let m = match self {
                Escapes::Aterm => _mm_or_si128(
                    quote,
                    _mm_or_si128(eq(b'\t'), _mm_or_si128(eq(b'\n'), eq(b'\r'))),
                ),
                // unsigned x < 0x20 iff min(x, 0x1f) == x
                Escapes::Json => _mm_or_si128(
                    quote,
                    _mm_cmpeq_epi8(_mm_min_epu8(x, _mm_set1_epi8(0x1f)), x),
                ),
            };
            u64::from(_mm_movemask_epi8(m) as u16)
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn mask(self, block: &[u8; 16]) -> u64 {
        // SAFETY: NEON is part of aarch64; the load reads exactly the 16
        // bytes of `block`.
        unsafe {
            let x = vld1q_u8(block.as_ptr());
            let eq = |c: u8| vceqq_u8(x, vdupq_n_u8(c));
            let quote = vorrq_u8(eq(b'"'), eq(b'\\'));
            let m = match self {
                Escapes::Aterm => {
                    vorrq_u8(quote, vorrq_u8(eq(b'\t'), vorrq_u8(eq(b'\n'), eq(b'\r'))))
                }
                Escapes::Json => vorrq_u8(quote, vcltq_u8(x, vdupq_n_u8(0x20))),
            };
            // byte k of `m` becomes nibble k; keep one bit of each
            let nibbles = vshrn_n_u16::<4>(vreinterpretq_u16_u8(m));
            vget_lane_u64::<0>(vreinterpret_u64_u8(nibbles)) & 0x8888_8888_8888_8888
        }
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    fn mask(self, block: &[u8; 16]) -> u64 {
        block
            .iter()
            .enumerate()
            .fold(0, |m, (i, &b)| m | (u64::from(self.hit(b)) << i))
    }

    /// `f` gets the positions of the bytes to escape, ascending.
    #[inline]
    pub fn for_each(self, s: &[u8], mut f: impl FnMut(usize)) {
        let emit = |mut bits: u64, base: usize, f: &mut dyn FnMut(usize)| {
            while bits != 0 {
                f(base + (bits.trailing_zeros() / BITS) as usize);
                bits &= bits - 1;
            }
        };
        let mut next = 0;
        while let Some(block) = s.get(next..).and_then(<[u8]>::first_chunk::<16>) {
            emit(self.mask(block), next, &mut f);
            next += 16;
        }
        let rem = s.len() - next;
        if rem == 0 {
            return;
        }
        if s.len() >= 16 {
            let block = s[s.len() - 16..].first_chunk::<16>().unwrap();
            emit(self.mask(block) >> ((16 - rem) as u32 * BITS), next, &mut f);
        } else {
            // `a` is in neither set
            let mut block = [b'a'; 16];
            block[..rem].copy_from_slice(s);
            emit(self.mask(&block), 0, &mut f);
        }
    }
}

/// Appends `s` as a JSON string: quoted, with an escape for every byte in
/// the `Json` set. Every such byte is ASCII, so the runs between them copy.
pub fn json_str(out: &mut Vec<u8>, st: &str) {
    out.push(b'"');
    let s = st.as_bytes();
    let mut done = 0;
    Escapes::Json.for_each(s, |i| {
        out.extend_from_slice(&s[done..i]);
        match s[i] {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            c => out.extend_from_slice(format!("\\u{c:04x}").as_bytes()),
        }
        done = i + 1;
    });
    out.extend_from_slice(&s[done..]);
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::Escapes;
    use rstest::rstest;

    fn positions(set: Escapes, s: &[u8]) -> Vec<usize> {
        let mut v = Vec::new();
        set.for_each(s, |i| v.push(i));
        v
    }

    fn slow(set: Escapes, s: &[u8]) -> Vec<usize> {
        (0..s.len()).filter(|&i| set.hit(s[i])).collect()
    }

    #[rstest]
    fn finds_every_byte_to_escape(#[values(Escapes::Aterm, Escapes::Json)] set: Escapes) {
        let mut s = [b'a'; 40];
        for at in 0..s.len() {
            for b in 0..=255u8 {
                s[at] = b;
                for end in at..=s.len() {
                    let got: Vec<usize> = positions(set, &s[..end]);
                    assert_eq!(got, slow(set, &s[..end]), "{b:#x} at {at}");
                }
            }
            s[at] = 0xff;
        }
        let mut x = 1u64;
        for _ in 0..10_000 {
            let s: Vec<u8> = (0..x % 70)
                .map(|_| {
                    x = x
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    // mostly bytes around the escaped ones
                    (x >> 59) as u8 * 5 + b'\t'
                })
                .collect();
            assert_eq!(positions(set, &s), slow(set, &s));
        }
    }
}
